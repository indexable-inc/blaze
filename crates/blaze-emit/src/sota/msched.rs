#![cfg_attr(not(test), forbid(unsafe_code))]
//! Data-Driven Target Model, Pre-RA Pressure-Aware & Bounded Beam-4 Machine Scheduler (`msched`)
//! over typed [`super::mfunc::MFunc`].
//!
//! Replaces both post-RA `mach_sched_m5` (which had to disassemble `u32` words) and fragile
//! pre-ISel AArch64 pressure heuristics with a single target-driven machine scheduler operating
//! directly on typed SoA `MFunc`:
//! 1. **Data-Driven [`TargetModel`]**: one code path parameterized for both Apple Silicon Macs
//!    ([`TargetModel::APPLE_M5`]), 128-core Graviton 5 / Neoverse V3 Linux servers
//!    ([`TargetModel::GRAVITON5_NEOVERSE_V3`]), derivable from [`blaze_tier::Machine`], and
//!    overridable from an on-device µbench calibrator via [`TargetModel::calibrated`].
//! 2. **Exact Loop-Carried Recurrence Analysis**: identifies loop backedges `b -> h` from
//!    `block_order` + `block_succs` (with natural-loop liveness over both `VReg`s and `PReg`s,
//!    including latch [`MOp::ParallelCopy`] feeds) and boosts recurrence-closing updates by
//!    `+12` height so critical loop-carried state updates (`bits_consumed`, `next_fse_state`,
//!    `src_ptr`, loop IVs) schedule immediately instead of sinking to height `0`.
//! 3. **Pre-RA SSA Two-Level Register-Pressure-Aware Scheduling (`func.is_ssa == true`)**: tracks
//!    exact per-window `VReg` remaining-use counts (`rem_uses`) and live GPR/FP pressure
//!    (`cur_gpr_live`, `cur_fp_live`) across window boundaries; in moderate pressure
//!    (`cur_gpr_live >= 18` or `cur_fp_live >= 20`), hoists critical loop-carried recurrences
//!    ahead of register-freeing ops (`pressure_delta < 0`); in critical spill-prevention mode
//!    (`cur_gpr_live >= 21` or `cur_fp_live >= 22`), prioritizes VReg-killing (`delta < 0`) and
//!    pressure-neutral (`delta == 0`) ops strictly ahead of pressure-increasing recurrences.
//! 4. **Post-RA Bounded Optimal Beam-4 Lookahead (`!func.is_ssa`, `4..=96` loop windows)**:
//!    evaluates up to `MAX_BEAM_STATES = 4` schedules with 2-step lookahead and exact per-pipe
//!    makespan + recurrence completion + macro-op fusion simulation, emitting whichever of beam-4
//!    or greedy achieves strictly lower simulated cycles (strictly `O(N)` bounded, zero heap).
//! 5. **Exact Apple M5 / Graviton 5 Macro-Op Fusion**: keeps `cmp`/`tst`/`adds`/`subs`/`ands` +
//!    `b.cc`, `adrp` + `addlo12`/`ldr [base, #off]`, and adjacent `and`/`ubfm` + `cbz`/`cbnz`
//!    glued while reordering all five SoA columns (`ops`, `flags`, `vflag_def`, `vflag_use`,
//!    `note_idx`) in lockstep with zero note remapping.

use blaze_own::{Alloc, Vec};
use blaze_tier::{Machine, Os};

use super::mfunc::{
    AddrMode, FpAluOp, LogicalKind, MBlock, MCond, MFunc, MInstFlags, MNoteKind, MOp, MReg,
    NO_NOTE, NO_VFLAG, PReg, VecAluOp,
};

/// Maximum instructions scheduled in one list-scheduling window (strict O(N * 128) = O(N), Rule 18).
const WINDOW_MAX: usize = 128;

/// Height bonus applied to loop-carried recurrence-closing instructions so critical backedge
/// updates schedule immediately once their inputs are ready instead of sinking to height `0`.
const RECURRENCE_HEIGHT_BOOST: u16 = 12;

/// Maximum fixed-point iterations for natural-loop / SSA backward liveness (Rule 18 bounded progress).
const MAX_LOOP_LIVENESS_ITERS: usize = 32;

/// Pre-RA SSA GPR moderate live-pressure threshold: at `18..21` live GPRs, prioritize
/// loop-carried recurrences and VReg-killing instructions ahead of non-critical new defs.
const SSA_GPR_PRESSURE_THRESHOLD: u16 = 18;

/// Pre-RA SSA GPR critical spill-prevention threshold: at `>= 21` live GPRs (approaching the
/// 27-GPR leaf limit), prioritize VReg-killing (`delta < 0`) and pressure-neutral (`delta == 0`)
/// ops strictly ahead of pressure-increasing recurrences to prevent live-range spikes and spills.
const SSA_GPR_CRITICAL_THRESHOLD: u16 = 21;

/// Pre-RA SSA FP/SIMD moderate live-pressure threshold (leaving headroom below 24 caller-saved FP regs).
const SSA_FP_PRESSURE_THRESHOLD: u16 = 20;

/// Pre-RA SSA FP/SIMD critical spill-prevention threshold.
const SSA_FP_CRITICAL_THRESHOLD: u16 = 22;

/// Maximum `n_blocks * num_vwords` entries allocated for cross-block SSA backward liveness (512 KiB).
const MAX_SSA_LIVENESS_WORDS: usize = 65_536;

/// Minimum window length eligible for post-RA bounded Beam-4 lookahead search.
const BEAM_MIN_LEN: usize = 4;

/// Maximum window length eligible for post-RA bounded Beam-4 lookahead search (keeps `BeamState`
/// compact and compile time strictly bounded on >96-inst windows while the O(N) list scheduler
/// spans the full 128-inst `WINDOW_MAX`).
const BEAM_MAX_LEN: usize = 96;

/// Maximum beam frontier states kept per step (strictly O(N) bounded, stack-only, Rule 18).
const MAX_BEAM_STATES: usize = 4;

/// Maximum ready candidates expanded per beam state per step.
const MAX_BEAM_BRANCH: usize = 4;

/// Sentinel window-local instruction index meaning "no previous instruction".
const NO_LOCAL_OP: u8 = u8::MAX;

// -----------------------------------------------------------------------------
// Zero-`as` numeric helpers
// -----------------------------------------------------------------------------

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn u8_of(v: usize) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
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
fn set<T>(s: &mut [T], i: usize, val: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = val;
    }
}

#[inline]
fn bitor_at<T: core::ops::BitOrAssign>(s: &mut [T], i: usize, bits: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot |= bits;
    }
}

#[inline]
fn add_at_u8(s: &mut [u8], i: usize, delta: u8) {
    if let Some(slot) = s.get_mut(i) {
        *slot = slot.saturating_add(delta);
    }
}

#[inline]
fn max_at_u16(s: &mut [u16], i: usize, val: u16) {
    if let Some(slot) = s.get_mut(i)
        && val > *slot
    {
        *slot = val;
    }
}

// -----------------------------------------------------------------------------
// 1. Data-Driven `TargetModel`
// -----------------------------------------------------------------------------

/// Measured microarchitectural parameters for [`TargetModel::calibrated`] (Rule 83).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CalibratedTargetOpts {
    /// Baseline ISA & microarchitecture model supplying ISA flags, page size, and fusion defaults.
    pub base: TargetModel,
    /// Measured front-end decode/dispatch width (instructions/cycle; clamped to `>= 1`).
    pub decode_width: u8,
    /// Measured integer ALU execution pipes per cycle (clamped to `>= 1`).
    pub alu_pipes: u8,
    /// Measured load/store execution pipes per cycle (clamped to `>= 1`).
    pub ldst_pipes: u8,
    /// Measured FP/NEON SIMD execution pipes per cycle (clamped to `>= 1`).
    pub fp_pipes: u8,
    /// Measured integer multiply execution pipes per cycle (clamped to `>= 1`).
    pub mul_pipes: u8,
    /// Measured L1 load-to-use latency in cycles (clamped to `>= 1`).
    pub load_latency: u8,
    /// Measured 64-bit integer multiply latency in cycles (clamped to `>= 1`).
    pub mul_latency: u8,
    /// Measured scalar FP / NEON ALU latency in cycles (clamped to `>= 1`).
    pub fp_latency: u8,
    /// Measured branch misprediction penalty in cycles (clamped to `>= 1`).
    pub branch_mispredict_cycles: u8,
}

impl CalibratedTargetOpts {
    /// Initializes calibration options from `base` so callers may override measured fields via
    /// struct update syntax (`CalibratedTargetOpts { load_latency: 5, ..CalibratedTargetOpts::from_base(base) }`).
    #[inline]
    pub const fn from_base(base: TargetModel) -> Self {
        Self {
            base,
            decode_width: base.decode_width,
            alu_pipes: base.alu_pipes,
            ldst_pipes: base.ldst_pipes,
            fp_pipes: base.fp_pipes,
            mul_pipes: base.mul_pipes,
            load_latency: base.load_latency,
            mul_latency: base.mul_latency,
            fp_latency: base.fp_latency,
            branch_mispredict_cycles: base.branch_mispredict_cycles,
        }
    }
}

/// Microarchitectural and ISA model driving AArch64 machine scheduling across Apple Silicon Macs
/// and Linux Arm64 servers (Graviton 5 / Neoverse V3) through one shared code path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TargetModel {
    pub decode_width: u8,
    pub alu_pipes: u8,
    pub ldst_pipes: u8,
    pub fp_pipes: u8,
    pub mul_pipes: u8,
    pub load_latency: u8,
    pub mul_latency: u8,
    pub fp_latency: u8,
    pub branch_mispredict_cycles: u8,
    pub macro_fuse_cmp_br: bool,
    pub macro_fuse_adrp_add: bool,
    pub page_size: u32,
    pub has_cssc: bool,
    pub has_lse: bool,
    pub has_sve2: bool,
}

impl TargetModel {
    /// Apple Silicon M5 (macOS arm64): 10-wide decode, 6 ALU / 3 LDST / 4 FP / 2 MUL, 16 KiB pages.
    pub const APPLE_M5: Self = Self {
        decode_width: 10,
        alu_pipes: 6,
        ldst_pipes: 3,
        fp_pipes: 4,
        mul_pipes: 2,
        load_latency: 4,
        mul_latency: 3,
        fp_latency: 3,
        branch_mispredict_cycles: 14,
        macro_fuse_cmp_br: true,
        macro_fuse_adrp_add: true,
        page_size: 16384,
        has_cssc: true,
        has_lse: true,
        has_sve2: false,
    };

    /// AWS Graviton 5 / Arm Neoverse V3 (Linux arm64): 8-wide decode, 6 ALU / 3 LDST / 4 FP / 2 MUL,
    /// 4 KiB pages, SVE2 enabled.
    pub const GRAVITON5_NEOVERSE_V3: Self = Self {
        decode_width: 8,
        alu_pipes: 6,
        ldst_pipes: 3,
        fp_pipes: 4,
        mul_pipes: 2,
        load_latency: 4,
        mul_latency: 3,
        fp_latency: 3,
        branch_mispredict_cycles: 12,
        macro_fuse_cmp_br: true,
        macro_fuse_adrp_add: true,
        page_size: 4096,
        has_cssc: true,
        has_lse: true,
        has_sve2: true,
    };

    /// Derives a [`TargetModel`] from a [`blaze_tier::Machine`] target descriptor.
    #[inline]
    pub const fn from_machine(machine: Machine) -> Self {
        let mut model = match machine.os {
            Os::MacOs => Self::APPLE_M5,
            Os::Linux => Self::GRAVITON5_NEOVERSE_V3,
        };
        model.has_lse = (machine.features & Machine::LSE) != 0;
        model.has_cssc = (machine.features & Machine::CSSC) != 0;
        model
    }

    /// Constructs a [`TargetModel`] with widths and latencies supplied by an on-device µbench
    /// calibrator while preserving ISA feature flags, page size, and fusion defaults from
    /// `opts.base` (Rule 83). Zero values are clamped to `1`.
    #[inline]
    pub const fn calibrated(opts: CalibratedTargetOpts) -> Self {
        let mut model = opts.base;
        model.decode_width = if opts.decode_width == 0 { 1 } else { opts.decode_width };
        model.alu_pipes = if opts.alu_pipes == 0 { 1 } else { opts.alu_pipes };
        model.ldst_pipes = if opts.ldst_pipes == 0 { 1 } else { opts.ldst_pipes };
        model.fp_pipes = if opts.fp_pipes == 0 { 1 } else { opts.fp_pipes };
        model.mul_pipes = if opts.mul_pipes == 0 { 1 } else { opts.mul_pipes };
        model.load_latency = if opts.load_latency == 0 { 1 } else { opts.load_latency };
        model.mul_latency = if opts.mul_latency == 0 { 1 } else { opts.mul_latency };
        model.fp_latency = if opts.fp_latency == 0 { 1 } else { opts.fp_latency };
        model.branch_mispredict_cycles =
            if opts.branch_mispredict_cycles == 0 { 1 } else { opts.branch_mispredict_cycles };
        model
    }
}

/// Summary statistics returned by [`schedule_mfunc`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MSchedStats {
    pub regions_scheduled: u32,
    pub insts_reordered: u32,
    pub recurrences_boosted: u32,
}

// -----------------------------------------------------------------------------
// 2. Instruction Classification (Registers, Memory, Flags, Pipe, Latency)
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExecPipe {
    Alu,
    LdSt,
    Fp,
    Mul,
}

impl ExecPipe {
    #[inline]
    const fn index(self) -> usize {
        match self {
            Self::Alu => 0,
            Self::LdSt => 1,
            Self::Fp => 2,
            Self::Mul => 3,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MemKind {
    None,
    Load,
    Store,
    Barrier,
}

/// Compact per-instruction dependency and resource metadata for one schedulable `MOp`.
#[derive(Clone, Copy, Debug)]
struct InstInfo {
    /// Physical GPR mask: bits `0..=30` = `X0..=X30`, bit `31` = `SP` (`XZR` sets 0).
    phys_gpr_defs: u32,
    phys_gpr_uses: u32,
    /// Physical SIMD/FP mask: bits `0..=31` = `V0..=V31`.
    phys_fp_defs: u32,
    phys_fp_uses: u32,
    /// Virtual register defs (`VReg.0`), max 3 (e.g. `Ldp` with writeback).
    vreg_defs: [u32; 3],
    vreg_def_len: u8,
    /// Virtual register uses (`VReg.0`), max 4.
    vreg_uses: [u32; 4],
    vreg_use_len: u8,
    writes_flags: bool,
    reads_flags: bool,
    mem: MemKind,
    may_trap: bool,
    pipe: ExecPipe,
    lat: u8,
}

impl InstInfo {
    const EMPTY: Self = Self {
        phys_gpr_defs: 0,
        phys_gpr_uses: 0,
        phys_fp_defs: 0,
        phys_fp_uses: 0,
        vreg_defs: [0; 3],
        vreg_def_len: 0,
        vreg_uses: [0; 4],
        vreg_use_len: 0,
        writes_flags: false,
        reads_flags: false,
        mem: MemKind::None,
        may_trap: false,
        pipe: ExecPipe::Alu,
        lat: 1,
    };

    #[inline]
    fn add_def(&mut self, reg: MReg) {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => {
                self.phys_gpr_defs |= 1u32 << r;
            }
            MReg::P(PReg::Sp) => {
                self.phys_gpr_defs |= 1u32 << 31;
            }
            MReg::P(PReg::V(r)) if r <= 31 => {
                self.phys_fp_defs |= 1u32 << r;
            }
            MReg::P(PReg::Nzcv) => {
                self.writes_flags = true;
            }
            MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Xzr) => {}
            MReg::V(v) => {
                let len = usize::from(self.vreg_def_len);
                for &existing in self.vreg_defs.get(..len).unwrap_or(&[]) {
                    if existing == v.0 {
                        return;
                    }
                }
                if let Some(slot) = self.vreg_defs.get_mut(len) {
                    *slot = v.0;
                    self.vreg_def_len = self.vreg_def_len.saturating_add(1);
                }
            }
        }
    }

    #[inline]
    fn add_use(&mut self, reg: MReg) {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => {
                self.phys_gpr_uses |= 1u32 << r;
            }
            MReg::P(PReg::Sp) => {
                self.phys_gpr_uses |= 1u32 << 31;
            }
            MReg::P(PReg::V(r)) if r <= 31 => {
                self.phys_fp_uses |= 1u32 << r;
            }
            MReg::P(PReg::Nzcv) => {
                self.reads_flags = true;
            }
            MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Xzr) => {}
            MReg::V(v) => {
                let len = usize::from(self.vreg_use_len);
                for &existing in self.vreg_uses.get(..len).unwrap_or(&[]) {
                    if existing == v.0 {
                        return;
                    }
                }
                if let Some(slot) = self.vreg_uses.get_mut(len) {
                    *slot = v.0;
                    self.vreg_use_len = self.vreg_use_len.saturating_add(1);
                }
            }
        }
    }

    #[inline]
    fn vreg_defs_slice(&self) -> &[u32] {
        self.vreg_defs.get(..usize::from(self.vreg_def_len)).unwrap_or(&[])
    }

    #[inline]
    fn vreg_uses_slice(&self) -> &[u32] {
        self.vreg_uses.get(..usize::from(self.vreg_use_len)).unwrap_or(&[])
    }

    #[inline]
    fn add_addr(&mut self, addr: AddrMode) {
        match addr {
            AddrMode::BaseOff { base, .. } => {
                self.add_use(base);
            }
            AddrMode::ScaledIdx { base, idx, .. } => {
                self.add_use(base);
                self.add_use(idx);
            }
            AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => {
                self.add_use(base);
                self.add_def(base);
            }
        }
    }
}

#[inline]
fn vreg_slices_intersect(a: &[u32], b: &[u32]) -> bool {
    for &va in a {
        for &vb in b {
            if va == vb {
                return true;
            }
        }
    }
    false
}

/// Classifies one instruction's register operands, memory effect, execution pipe, and latency.
fn classify_inst(func: &MFunc<'_>, idx: usize, target: TargetModel) -> InstInfo {
    let op = at(&func.ops, idx, MOp::Nop);
    let flags = at(&func.flags, idx, MInstFlags::NONE);
    let mut info = InstInfo::EMPTY;
    info.writes_flags = op.writes_flags() || at(&func.vflag_def, idx, NO_VFLAG) != NO_VFLAG;
    info.reads_flags = op.reads_flags() || at(&func.vflag_use, idx, NO_VFLAG) != NO_VFLAG;
    info.may_trap = flags.contains(MInstFlags::MAY_TRAP);

    let load_lat = target.load_latency.max(1);
    let mul_lat = target.mul_latency.max(1);
    let fp_lat = target.fp_latency.max(1);

    match op {
        MOp::MovImm { dst, .. } | MOp::Adrp { dst } | MOp::AdrTable { dst, .. } => {
            info.add_def(dst);
        }
        MOp::Mov { dst, src: rn, .. }
        | MOp::MovSp { dst, src: rn }
        | MOp::AddImm { dst, rn, .. }
        | MOp::SubImm { dst, rn, .. }
        | MOp::LogicalImm { dst, rn, .. }
        | MOp::Ubfm { dst, rn, .. }
        | MOp::Sbfm { dst, rn, .. }
        | MOp::Rbit { dst, rn, .. }
        | MOp::Clz { dst, rn, .. }
        | MOp::Rev { dst, rn, .. }
        | MOp::AddLo12 { dst, rn }
        | MOp::Mvn { dst, rm: rn, .. } => {
            info.add_def(dst);
            info.add_use(rn);
        }
        MOp::Bfxil { dst, rn, .. } => {
            // `bfxil` preserves unmodified bits of `dst`, so it both reads and writes `dst`.
            info.add_use(dst);
            info.add_use(rn);
            info.add_def(dst);
        }
        MOp::AddReg { dst, rn, rm, .. }
        | MOp::SubReg { dst, rn, rm, .. }
        | MOp::AddExt { dst, rn, rm, .. }
        | MOp::SubExt { dst, rn, rm, .. }
        | MOp::LogicalReg { dst, rn, rm, .. }
        | MOp::ShiftVar { dst, rn, rm, .. }
        | MOp::Extr { dst, rn, rm, .. }
        | MOp::Umin { dst, rn, rm, .. }
        | MOp::Umax { dst, rn, rm, .. }
        | MOp::Smin { dst, rn, rm, .. }
        | MOp::Smax { dst, rn, rm, .. }
        | MOp::Csel { dst, rn, rm, .. }
        | MOp::Csinc { dst, rn, rm, .. }
        | MOp::Csinv { dst, rn, rm, .. }
        | MOp::Csneg { dst, rn, rm, .. }
        | MOp::CarryAddSub { dst, rn, rm, .. } => {
            info.add_def(dst);
            info.add_use(rn);
            info.add_use(rm);
        }
        MOp::Mul { dst, rn, rm, .. }
        | MOp::Umulh { dst, rn, rm }
        | MOp::Smulh { dst, rn, rm } => {
            info.add_def(dst);
            info.add_use(rn);
            info.add_use(rm);
            info.pipe = ExecPipe::Mul;
            info.lat = mul_lat;
        }
        MOp::Madd { dst, rn, rm, ra, .. } | MOp::Msub { dst, rn, rm, ra, .. } => {
            info.add_def(dst);
            info.add_use(rn);
            info.add_use(rm);
            info.add_use(ra);
            info.pipe = ExecPipe::Mul;
            info.lat = mul_lat;
        }
        MOp::Udiv { dst, rn, rm, .. } | MOp::Sdiv { dst, rn, rm, .. } => {
            info.add_def(dst);
            info.add_use(rn);
            info.add_use(rm);
            info.pipe = ExecPipe::Mul;
            info.lat = mul_lat.saturating_mul(3);
        }
        MOp::CmpImm { rn: reg, .. }
        | MOp::TstImm { rn: reg, .. }
        | MOp::CcmpImm { rn: reg, .. }
        | MOp::Blr { target: reg }
        | MOp::Br { target: reg }
        | MOp::Cbz { rt: reg, .. }
        | MOp::Cbnz { rt: reg, .. }
        | MOp::Tbz { rt: reg, .. }
        | MOp::Tbnz { rt: reg, .. } => {
            info.add_use(reg);
        }
        MOp::CmpReg { rn, rm, .. } | MOp::TstReg { rn, rm, .. } | MOp::CcmpReg { rn, rm, .. } => {
            info.add_use(rn);
            info.add_use(rm);
        }
        MOp::Ldr { dst, addr, .. } => {
            info.add_addr(addr);
            info.add_def(dst);
            info.mem = MemKind::Load;
            info.pipe = ExecPipe::LdSt;
            info.lat = load_lat;
        }
        MOp::Str { src, addr, .. } => {
            info.add_use(src);
            info.add_addr(addr);
            info.mem = MemKind::Store;
            info.pipe = ExecPipe::LdSt;
            info.lat = 1;
        }
        MOp::Ldp { dst0, dst1, base, post_idx, pre_idx, .. } => {
            info.add_use(base);
            info.add_def(dst0);
            info.add_def(dst1);
            if post_idx || pre_idx {
                info.add_def(base);
            }
            info.mem = MemKind::Load;
            info.pipe = ExecPipe::LdSt;
            info.lat = load_lat;
        }
        MOp::Stp { src0, src1, base, post_idx, pre_idx, .. } => {
            info.add_use(src0);
            info.add_use(src1);
            info.add_use(base);
            if post_idx || pre_idx {
                info.add_def(base);
            }
            info.mem = MemKind::Store;
            info.pipe = ExecPipe::LdSt;
            info.lat = 1;
        }
        MOp::FpAlu { op: fp_op, dst, rn, rm, ra, .. } => {
            info.add_def(dst);
            info.add_use(rn);
            if let Some(m) = rm {
                info.add_use(m);
            }
            if let Some(a) = ra {
                info.add_use(a);
            }
            info.pipe = ExecPipe::Fp;
            info.lat = if matches!(fp_op, FpAluOp::Div | FpAluOp::Sqrt) {
                fp_lat.saturating_mul(3)
            } else if matches!(fp_op, FpAluOp::Madd | FpAluOp::Msub) {
                fp_lat.saturating_add(1)
            } else {
                fp_lat
            };
        }
        MOp::FpCmp { rn, rm, .. } => {
            info.add_use(rn);
            info.add_use(rm);
            info.pipe = ExecPipe::Fp;
            info.lat = fp_lat;
        }
        MOp::Fcsel { dst, rn, rm, .. } => {
            info.add_def(dst);
            info.add_use(rn);
            info.add_use(rm);
            info.pipe = ExecPipe::Fp;
            info.lat = fp_lat;
        }
        MOp::VecAlu { op: vec_op, dst, rn, rm, .. } => {
            if !matches!(vec_op, VecAluOp::Zero) {
                info.add_use(rn);
            }
            if let Some(m) = rm {
                info.add_use(m);
            }
            if matches!(
                vec_op,
                VecAluOp::Bsl
                    | VecAluOp::Bit
                    | VecAluOp::Bif
                    | VecAluOp::Mla
                    | VecAluOp::Mls
                    | VecAluOp::Fmla
                    | VecAluOp::Fmls
            ) {
                info.add_use(dst);
            }
            info.add_def(dst);
            info.pipe = ExecPipe::Fp;
            info.lat = if matches!(
                vec_op,
                VecAluOp::Mul
                    | VecAluOp::Mla
                    | VecAluOp::Mls
                    | VecAluOp::Fmul
                    | VecAluOp::Fmla
                    | VecAluOp::Fmls
            ) {
                fp_lat.saturating_add(1)
            } else {
                fp_lat
            };
        }
        MOp::RawWord { defs_gpr, uses_gpr, defs_fp, uses_fp, is_mem_barrier, .. } => {
            info.phys_gpr_defs = defs_gpr;
            info.phys_gpr_uses = uses_gpr;
            info.phys_fp_defs = defs_fp;
            info.phys_fp_uses = uses_fp;
            if is_mem_barrier {
                info.mem = MemKind::Barrier;
            }
        }
        MOp::Prfm { base, .. } => {
            info.add_use(base);
            info.mem = MemKind::Load;
        }
        MOp::Bl
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Ret
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop
        | MOp::ParallelCopy { .. } => {}
    }

    // Reconcile memory effect with explicit `MInstFlags` attributes (taking the stronger effect).
    if flags.contains(MInstFlags::MEM_BARRIER) {
        info.mem = MemKind::Barrier;
    } else if flags.contains(MInstFlags::MEM_WRITE) && !matches!(info.mem, MemKind::Barrier) {
        info.mem = MemKind::Store;
    } else if flags.contains(MInstFlags::MEM_READ) && matches!(info.mem, MemKind::None) {
        info.mem = MemKind::Load;
    }

    info
}

/// Returns `true` if instruction `idx` must act as a scheduling barrier (never reordered across).
fn is_sched_barrier(func: &MFunc<'_>, idx: usize) -> bool {
    let flags = at(&func.flags, idx, MInstFlags::NONE);
    if flags.contains(MInstFlags::PINNED)
        || flags.contains(MInstFlags::VOLATILE)
        || flags.contains(MInstFlags::MEM_BARRIER)
        || flags.contains(MInstFlags::CALL)
        || flags.contains(MInstFlags::TERMINATOR)
    {
        return true;
    }
    let Some(op) = func.ops.get(idx) else {
        return true;
    };
    if op.is_unconditional_terminator() || op.is_conditional_branch() {
        return true;
    }
    if matches!(
        *op,
        MOp::Bl
            | MOp::Blr { .. }
            | MOp::ParallelCopy { .. }
            | MOp::RawWord { is_mem_barrier: true, .. }
    ) {
        return true;
    }

    // Check side-band notes for volatile or jump-table anchor constraints.
    let n_notes = func.notes.len();
    let mut cur = at(&func.note_idx, idx, NO_NOTE);
    let mut steps = 0usize;
    while cur != NO_NOTE && steps < n_notes {
        let Some(note) = func.notes.get(to_idx(cur)) else {
            break;
        };
        if matches!(note.kind, MNoteKind::Volatile | MNoteKind::JumpTableAnchor { .. }) {
            return true;
        }
        cur = note.next;
        steps += 1;
    }
    false
}

/// Returns `true` if instruction `op` is a conditional flag-reading branch (`b.cc`, not `al`/`nv`).
#[inline]
fn is_flag_cond_branch(op: &MOp) -> bool {
    matches!(*op, MOp::BCond { cond, .. } if !matches!(cond, MCond::Al | MCond::Nv))
}

/// Returns `true` if `op` macro-op fuses with an immediately following `BCond` on Apple Silicon
/// and Arm Neoverse V3 (`cmp`/`tst`/`adds`/`subs`/`ands`).
#[inline]
fn is_cmp_br_fusible(op: &MOp) -> bool {
    matches!(
        *op,
        MOp::CmpImm { .. }
            | MOp::CmpReg { .. }
            | MOp::TstImm { .. }
            | MOp::TstReg { .. }
            | MOp::AddImm { sets_flags: true, .. }
            | MOp::SubImm { sets_flags: true, .. }
            | MOp::AddReg { sets_flags: true, .. }
            | MOp::SubReg { sets_flags: true, .. }
            | MOp::LogicalImm { kind: LogicalKind::Ands, .. }
            | MOp::LogicalReg { kind: LogicalKind::Ands, .. }
    )
}

/// Returns `true` if `first` (`adrp`) and `second` (`addlo12` or `ldr [base, #off]`) macro-op
/// fuse on Apple Silicon / Neoverse V3 through the same base register.
#[inline]
fn is_adrp_fusion_pair(first: &MOp, second: &MOp) -> bool {
    let MOp::Adrp { dst } = *first else {
        return false;
    };
    match *second {
        MOp::AddLo12 { rn, .. } => rn == dst,
        MOp::Ldr { addr: AddrMode::BaseOff { base, .. }, .. } => base == dst,
        _ => false,
    }
}

/// Returns the tested register if `op` is `Cbz` or `Cbnz`.
#[inline]
fn cbz_tested_reg(op: &MOp) -> Option<MReg> {
    match *op {
        MOp::Cbz { rt, .. } | MOp::Cbnz { rt, .. } => Some(rt),
        _ => None,
    }
}

/// Returns `true` if `op` (`and` or `ubfm`) macro-op fuses with an adjacent `Cbz`/`Cbnz` testing `rt`.
#[inline]
fn is_cbz_fusible(op: &MOp, rt: MReg) -> bool {
    match *op {
        MOp::LogicalReg { dst, kind: LogicalKind::And, .. } | MOp::Ubfm { dst, .. } => dst == rt,
        _ => false,
    }
}

/// If `op` is a pre-RA SSA pointer-advance step (`AddImm`/`SubImm`/`AddReg`/`SubReg`/
/// `AddExt`/`SubExt` defining a distinct virtual register from one or two candidate virtual base
/// registers), returns the candidate base `VReg` ids.
#[inline]
fn ssa_ptr_step_bases(op: &MOp) -> (Option<u32>, Option<u32>) {
    match *op {
        MOp::AddImm { dst: MReg::V(d), rn: MReg::V(base), .. }
        | MOp::SubImm { dst: MReg::V(d), rn: MReg::V(base), .. }
        | MOp::SubReg { dst: MReg::V(d), rn: MReg::V(base), .. }
        | MOp::AddExt { dst: MReg::V(d), rn: MReg::V(base), .. }
        | MOp::SubExt { dst: MReg::V(d), rn: MReg::V(base), .. }
            if d != base =>
        {
            (Some(base.0), None)
        }
        MOp::AddReg { dst: MReg::V(d), rn, rm, .. } => {
            let b0 = match rn {
                MReg::V(v) if v != d => Some(v.0),
                _ => None,
            };
            let b1 = match rm {
                MReg::V(v) if v != d && Some(v.0) != b0 => Some(v.0),
                _ => None,
            };
            (b0, b1)
        }
        _ => (None, None),
    }
}

/// If `op` is a memory op (`Ldr`/`Str`/`Ldp`/`Stp`) reading a virtual address base register
/// `base`, returns `Some(base.0)`.
#[inline]
fn mem_base_vreg(op: &MOp) -> Option<u32> {
    match *op {
        MOp::Ldr { addr, .. } | MOp::Str { addr, .. } => match addr {
            AddrMode::BaseOff { base: MReg::V(b), .. }
            | AddrMode::ScaledIdx { base: MReg::V(b), .. }
            | AddrMode::PostIdx { base: MReg::V(b), .. }
            | AddrMode::PreIdx { base: MReg::V(b), .. } => Some(b.0),
            AddrMode::BaseOff { .. }
            | AddrMode::ScaledIdx { .. }
            | AddrMode::PostIdx { .. }
            | AddrMode::PreIdx { .. } => None,
        },
        MOp::Ldp { base: MReg::V(b), .. } | MOp::Stp { base: MReg::V(b), .. } => Some(b.0),
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// 3. Exact Loop-Carried Recurrence Analysis (Both `VReg`s and `PReg`s)
// -----------------------------------------------------------------------------

/// Hybrid physical (`u32` GPR + `u32` FP) + virtual (`[u64; vwords]`) register set with
/// O(touched) `clear()` via `dirty_words` so per-block resets never scan empty megabyte bitsets.
struct HybridRegSet<'a> {
    gpr: u32,
    fp: u32,
    vwords: Vec<u64, Alloc<'a>>,
    dirty_words: Vec<usize, Alloc<'a>>,
}

impl<'a> HybridRegSet<'a> {
    fn new(num_vwords: usize, alloc: Alloc<'a>) -> Self {
        Self {
            gpr: 0,
            fp: 0,
            vwords: vec_filled(num_vwords, 0u64, alloc),
            dirty_words: Vec::new_in(alloc),
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.gpr = 0;
        self.fp = 0;
        for &w in self.dirty_words.iter() {
            if let Some(slot) = self.vwords.get_mut(w) {
                *slot = 0;
            }
        }
        self.dirty_words.clear();
    }

    #[inline]
    fn insert_mreg(&mut self, reg: MReg) {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => self.gpr |= 1u32 << r,
            MReg::P(PReg::V(r)) if r <= 31 => self.fp |= 1u32 << r,
            MReg::V(v) => self.insert_vreg(v.0),
            MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv) => {}
        }
    }

    #[inline]
    fn remove_mreg(&mut self, reg: MReg) {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => self.gpr &= !(1u32 << r),
            MReg::P(PReg::V(r)) if r <= 31 => self.fp &= !(1u32 << r),
            MReg::V(v) => self.remove_vreg(v.0),
            MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv) => {}
        }
    }

    #[inline]
    fn contains_mreg(&self, reg: MReg) -> bool {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => (self.gpr & (1u32 << r)) != 0,
            MReg::P(PReg::V(r)) if r <= 31 => (self.fp & (1u32 << r)) != 0,
            MReg::V(v) => self.contains_vreg(v.0),
            MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv) => false,
        }
    }

    #[inline]
    fn insert_vreg(&mut self, id: u32) {
        let w = to_idx(id >> 6);
        if let Some(slot) = self.vwords.get_mut(w) {
            if *slot == 0 {
                self.dirty_words.push(w);
            }
            *slot |= 1u64 << (id & 63);
        }
    }

    #[inline]
    fn remove_vreg(&mut self, id: u32) {
        let w = to_idx(id >> 6);
        if let Some(slot) = self.vwords.get_mut(w) {
            *slot &= !(1u64 << (id & 63));
        }
    }

    #[inline]
    fn contains_vreg(&self, id: u32) -> bool {
        let w = to_idx(id >> 6);
        self.vwords.get(w).is_some_and(|&bits| (bits & (1u64 << (id & 63))) != 0)
    }

    /// Inserts all scalar/FP/virtual registers from `(gpr_mask, fp_mask, vregs)` (ignoring `SP` bit 31).
    #[inline]
    fn insert_info_mask(&mut self, gpr_mask: u32, fp_mask: u32, vregs: &[u32]) {
        self.gpr |= gpr_mask & 0x7fff_ffff;
        self.fp |= fp_mask;
        for &v in vregs {
            self.insert_vreg(v);
        }
    }

    /// Removes all scalar/FP/virtual registers from `(gpr_mask, fp_mask, vregs)`.
    #[inline]
    fn remove_info_mask(&mut self, gpr_mask: u32, fp_mask: u32, vregs: &[u32]) {
        self.gpr &= !(gpr_mask & 0x7fff_ffff);
        self.fp &= !fp_mask;
        for &v in vregs {
            self.remove_vreg(v);
        }
    }

    /// Returns `true` if any register in `(gpr_mask, fp_mask, vregs)` is present in `self`
    /// AND absent from `excluded`.
    #[inline]
    fn hits_unexcluded(
        &self,
        excluded: &Self,
        gpr_mask: u32,
        fp_mask: u32,
        vregs: &[u32],
    ) -> bool {
        let g = (gpr_mask & 0x7fff_ffff) & self.gpr & !excluded.gpr;
        if g != 0 {
            return true;
        }
        let f = fp_mask & self.fp & !excluded.fp;
        if f != 0 {
            return true;
        }
        for &v in vregs {
            if self.contains_vreg(v) && !excluded.contains_vreg(v) {
                return true;
            }
        }
        false
    }
}

/// Calls `f(succ_block)` for every CFG successor of `block`, preferring `func.block_succs` when
/// populated and falling back to scanning internal branch targets + `block_order` fallthrough.
fn for_each_block_succ(
    func: &MFunc<'_>,
    block: MBlock,
    order_pos: &[usize],
    mut f: impl FnMut(MBlock),
) {
    if !func.block_succs_off.is_empty() {
        for &s in func.block_succs(block) {
            f(s);
        }
        return;
    }
    let range = func.block_inst_range(block);
    let end = range.end.min(func.ops.len());
    let mut has_uncond = false;
    for op in func.ops.get(range.start..end).unwrap_or(&[]) {
        if let Some(target) = op.branch_target() {
            f(target);
        }
        if op.is_unconditional_terminator() {
            has_uncond = true;
        }
    }
    if !has_uncond {
        let b_idx = to_idx(block.0);
        if let Some(&pos) = order_pos.get(b_idx)
            && let Some(&next_blk) = pos.checked_add(1).and_then(|next| func.block_order.get(next))
        {
            f(next_blk);
        }
    }
}

/// Computes block-level `(upward_uses, block_defs)` for block `b_idx` into flat per-block arrays.
fn compute_block_use_def(
    func: &MFunc<'_>,
    b_idx: usize,
    slot: usize,
    num_vwords: usize,
    target: TargetModel,
    up_gpr: &mut [u32],
    up_fp: &mut [u32],
    up_v: &mut [u64],
    def_gpr: &mut [u32],
    def_fp: &mut [u32],
    def_v: &mut [u64],
) {
    let range = func.block_inst_range(MBlock(u32_of(b_idx)));
    let end = range.end.min(func.ops.len());
    let vbase = slot * num_vwords;

    let mut d_gpr = 0u32;
    let mut d_fp = 0u32;
    let mut u_gpr = 0u32;
    let mut u_fp = 0u32;

    for i in range.start..end {
        if let Some(&MOp::ParallelCopy { start, len }) = func.ops.get(i) {
            let cs = to_idx(start);
            let ce = cs.saturating_add(usize::from(len)).min(func.copy_pairs.len());
            let copy_slice = func.copy_pairs.get(cs..ce).unwrap_or(&[]);
            // Simultaneous parallel copy: check all `src` uses before recording any `dst` defs.
            for &(_, src) in copy_slice {
                match src {
                    MReg::P(PReg::Gpr(r)) if r <= 30 => {
                        let bit = 1u32 << r;
                        if (d_gpr & bit) == 0 {
                            u_gpr |= bit;
                        }
                    }
                    MReg::P(PReg::V(r)) if r <= 31 => {
                        let bit = 1u32 << r;
                        if (d_fp & bit) == 0 {
                            u_fp |= bit;
                        }
                    }
                    MReg::V(v) => {
                        let w = to_idx(v.0 >> 6);
                        let bit = 1u64 << (v.0 & 63);
                        if w < num_vwords && (at(def_v, vbase + w, 0u64) & bit) == 0 {
                            bitor_at(up_v, vbase + w, bit);
                        }
                    }
                    MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv) => {}
                }
            }
            for &(dst, _) in copy_slice {
                match dst {
                    MReg::P(PReg::Gpr(r)) if r <= 30 => d_gpr |= 1u32 << r,
                    MReg::P(PReg::V(r)) if r <= 31 => d_fp |= 1u32 << r,
                    MReg::V(v) => {
                        let w = to_idx(v.0 >> 6);
                        if w < num_vwords {
                            bitor_at(def_v, vbase + w, 1u64 << (v.0 & 63));
                        }
                    }
                    MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv) => {}
                }
            }
            continue;
        }

        let info = classify_inst(func, i, target);
        // Uses BEFORE defs (e.g. `add x1, x1, #8` is an upward-exposed read of `x1`).
        let ug = info.phys_gpr_uses & 0x7fff_ffff;
        let uf = info.phys_fp_uses;
        u_gpr |= ug & !d_gpr;
        u_fp |= uf & !d_fp;
        for &v in info.vreg_uses_slice() {
            let w = to_idx(v >> 6);
            let bit = 1u64 << (v & 63);
            if w < num_vwords && (at(def_v, vbase + w, 0u64) & bit) == 0 {
                bitor_at(up_v, vbase + w, bit);
            }
        }

        d_gpr |= info.phys_gpr_defs & 0x7fff_ffff;
        d_fp |= info.phys_fp_defs;
        for &v in info.vreg_defs_slice() {
            let w = to_idx(v >> 6);
            if w < num_vwords {
                bitor_at(def_v, vbase + w, 1u64 << (v & 63));
            }
        }
    }

    set(up_gpr, slot, u_gpr);
    set(up_fp, slot, u_fp);
    set(def_gpr, slot, d_gpr);
    set(def_fp, slot, d_fp);
}

/// Result of natural-loop & loop-carried recurrence analysis over an [`MFunc`].
struct LoopAnalysis<'a> {
    /// Per-instruction flag: `true` if the instruction closes a loop-carried recurrence.
    is_recurrence: Vec<bool, Alloc<'a>>,
    /// Per-block flag: `true` if the basic block lies inside any natural loop.
    in_loop_block: Vec<bool, Alloc<'a>>,
}

/// Identifies loop-carried recurrence-closing instructions and natural-loop blocks across all
/// loops in `func` (detected by backedges `b -> h` where `pos_in_order[h] <= pos_in_order[b]`).
fn analyze_loop_recurrences<'a>(
    func: &MFunc<'_>,
    target: TargetModel,
    alloc: Alloc<'a>,
) -> LoopAnalysis<'a> {
    let n_ops = func.ops.len();
    let n_blocks = func.block_insts.len();
    let mut is_recurrence = vec_filled(n_ops, false, alloc);
    let mut in_loop_block = vec_filled(n_blocks, false, alloc);

    if n_ops == 0 || n_blocks == 0 || func.block_order.is_empty() {
        return LoopAnalysis { is_recurrence, in_loop_block };
    }

    // Map each `MBlock` index to its position in `block_order`.
    let mut order_pos = vec_filled(n_blocks, usize::MAX, alloc);
    for (pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        if b < n_blocks && at(&order_pos, b, usize::MAX) == usize::MAX {
            set(&mut order_pos, b, pos);
        }
    }

    // 1. Find all loop backedges `b -> h` (`order_pos[h] <= order_pos[b]`).
    let mut backedges: Vec<(usize, usize), Alloc<'a>> = Vec::new_in(alloc);
    let mut is_header = vec_filled(n_blocks, false, alloc);
    let mut headers: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);

    for &blk in func.block_order.iter() {
        let b = to_idx(blk.0);
        if b >= n_blocks {
            continue;
        }
        let b_pos = at(&order_pos, b, usize::MAX);
        if b_pos == usize::MAX {
            continue;
        }
        for_each_block_succ(func, blk, &order_pos, |succ| {
            let h = to_idx(succ.0);
            if h < n_blocks && at(&order_pos, h, usize::MAX) <= b_pos {
                backedges.push((b, h));
                if !at(&is_header, h, false) {
                    set(&mut is_header, h, true);
                    headers.push(h);
                }
            }
        });
    }

    if headers.is_empty() {
        return LoopAnalysis { is_recurrence, in_loop_block };
    }

    let num_vregs = func.vreg_class.len();
    let num_vwords = num_vregs.div_ceil(64);
    let has_preds_csr = !func.block_preds_off.is_empty();

    // Reusable per-loop scratch buffers hoisted outside `headers` so mega-functions with
    // hundreds of loop headers never accumulate bump-arena allocations or scan all function
    // vregs per loop.
    let mut block_to_slot = vec_filled(n_blocks, usize::MAX, alloc);
    let mut loop_blocks: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut worklist: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut needed = HybridRegSet::new(num_vwords, alloc);
    let mut consumed_later = HybridRegSet::new(num_vwords, alloc);
    let mut copy_add_srcs: Vec<MReg, Alloc<'a>> = Vec::new_in(alloc);
    let mut copy_kill_dsts: Vec<MReg, Alloc<'a>> = Vec::new_in(alloc);
    let mut def_stamp = vec_filled(num_vregs, 0u32, alloc);
    let mut vreg_to_local = vec_filled(num_vregs, u32::MAX, alloc);
    let mut local_to_vreg: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut cur_stamp = 0u32;

    let mut up_gpr: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut up_fp: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut def_gpr: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut def_fp: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut live_in_gpr: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut live_in_fp: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut up_v: Vec<u64, Alloc<'a>> = Vec::new_in(alloc);
    let mut def_v: Vec<u64, Alloc<'a>> = Vec::new_in(alloc);
    let mut live_in_v: Vec<u64, Alloc<'a>> = Vec::new_in(alloc);

    for &h in headers.iter() {
        let h_pos = at(&order_pos, h, usize::MAX);
        loop_blocks.clear();
        worklist.clear();

        // 2. Collect natural loop body blocks for header `h`.
        set(&mut block_to_slot, h, 0usize);
        loop_blocks.push(h);

        let mut max_latch_pos = h_pos;
        for &(b, bh) in backedges.iter() {
            if bh != h {
                continue;
            }
            let b_ord = at(&order_pos, b, 0usize);
            if b_ord > max_latch_pos {
                max_latch_pos = b_ord;
            }
            if at(&block_to_slot, b, usize::MAX) == usize::MAX {
                let slot = loop_blocks.len();
                set(&mut block_to_slot, b, slot);
                loop_blocks.push(b);
                worklist.push(b);
            }
        }

        if has_preds_csr {
            while let Some(cur) = worklist.pop() {
                for &pred in func.block_preds(MBlock(u32_of(cur))) {
                    let p = to_idx(pred.0);
                    if p < n_blocks
                        && at(&order_pos, p, usize::MAX) >= h_pos
                        && at(&block_to_slot, p, usize::MAX) == usize::MAX
                    {
                        let slot = loop_blocks.len();
                        set(&mut block_to_slot, p, slot);
                        loop_blocks.push(p);
                        worklist.push(p);
                    }
                }
            }
        } else {
            // Fallback when `block_preds` CSR wasn't populated: include `block_order[h_pos..=max_latch_pos]`.
            let end_pos = max_latch_pos.min(func.block_order.len().saturating_sub(1));
            for &blk in func.block_order.get(h_pos..=end_pos).unwrap_or(&[]) {
                let p = to_idx(blk.0);
                if p < n_blocks && at(&block_to_slot, p, usize::MAX) == usize::MAX {
                    let slot = loop_blocks.len();
                    set(&mut block_to_slot, p, slot);
                    loop_blocks.push(p);
                }
            }
        }

        for &b_idx in loop_blocks.iter() {
            if b_idx < n_blocks {
                set(&mut in_loop_block, b_idx, true);
            }
        }

        let k_len = loop_blocks.len();

        // 2b. Compact loop-candidate `VReg`s: only vregs upward-exposed in at least one block of
        //     this loop can ever enter `live_in` of any block in the loop. Block-local SSA vregs
        //     are excluded without changing the recurrence solution.
        local_to_vreg.clear();
        if num_vregs > 0 {
            for &b_idx in loop_blocks.iter() {
                if cur_stamp == u32::MAX {
                    for s in def_stamp.iter_mut() {
                        *s = 0;
                    }
                    cur_stamp = 1;
                } else {
                    cur_stamp += 1;
                }
                let stamp = cur_stamp;
                let range = func.block_inst_range(MBlock(u32_of(b_idx)));
                let end = range.end.min(n_ops);
                for i in range.start..end {
                    if let Some(&MOp::ParallelCopy { start, len }) = func.ops.get(i) {
                        let cs = to_idx(start);
                        let ce = cs.saturating_add(usize::from(len)).min(func.copy_pairs.len());
                        let copy_slice = func.copy_pairs.get(cs..ce).unwrap_or(&[]);
                        for &(_, src) in copy_slice {
                            if let MReg::V(v) = src {
                                let vi = to_idx(v.0);
                                if vi < num_vregs
                                    && at(&def_stamp, vi, 0u32) != stamp
                                    && at(&vreg_to_local, vi, u32::MAX) == u32::MAX
                                {
                                    let lid = u32_of(local_to_vreg.len());
                                    set(&mut vreg_to_local, vi, lid);
                                    local_to_vreg.push(v.0);
                                }
                            }
                        }
                        for &(dst, _) in copy_slice {
                            if let MReg::V(v) = dst {
                                let vi = to_idx(v.0);
                                if vi < num_vregs {
                                    set(&mut def_stamp, vi, stamp);
                                }
                            }
                        }
                        continue;
                    }
                    let info = classify_inst(func, i, target);
                    for &v in info.vreg_uses_slice() {
                        let vi = to_idx(v);
                        if vi < num_vregs
                            && at(&def_stamp, vi, 0u32) != stamp
                            && at(&vreg_to_local, vi, u32::MAX) == u32::MAX
                        {
                            let lid = u32_of(local_to_vreg.len());
                            set(&mut vreg_to_local, vi, lid);
                            local_to_vreg.push(v);
                        }
                    }
                    for &v in info.vreg_defs_slice() {
                        let vi = to_idx(v);
                        if vi < num_vregs {
                            set(&mut def_stamp, vi, stamp);
                        }
                    }
                }
            }
        }

        let loop_vwords = local_to_vreg.len().div_ceil(64);
        let raw_total_vwords = k_len.saturating_mul(loop_vwords);
        let active_vwords = if raw_total_vwords <= MAX_SSA_LIVENESS_WORDS {
            loop_vwords
        } else {
            0
        };
        let total_vwords = k_len.saturating_mul(active_vwords);

        up_gpr.clear();
        up_gpr.resize(k_len, 0u32);
        up_fp.clear();
        up_fp.resize(k_len, 0u32);
        def_gpr.clear();
        def_gpr.resize(k_len, 0u32);
        def_fp.clear();
        def_fp.resize(k_len, 0u32);
        up_v.clear();
        up_v.resize(total_vwords, 0u64);
        def_v.clear();
        def_v.resize(total_vwords, 0u64);

        for (slot, &b_idx) in loop_blocks.iter().enumerate() {
            let range = func.block_inst_range(MBlock(u32_of(b_idx)));
            let end = range.end.min(n_ops);
            let vbase = slot * active_vwords;
            let mut d_gpr = 0u32;
            let mut d_fp = 0u32;
            let mut u_gpr = 0u32;
            let mut u_fp = 0u32;

            for i in range.start..end {
                if let Some(&MOp::ParallelCopy { start, len }) = func.ops.get(i) {
                    let cs = to_idx(start);
                    let ce = cs.saturating_add(usize::from(len)).min(func.copy_pairs.len());
                    let copy_slice = func.copy_pairs.get(cs..ce).unwrap_or(&[]);
                    for &(_, src) in copy_slice {
                        match src {
                            MReg::P(PReg::Gpr(r)) if r <= 30 => {
                                let bit = 1u32 << r;
                                if (d_gpr & bit) == 0 {
                                    u_gpr |= bit;
                                }
                            }
                            MReg::P(PReg::V(r)) if r <= 31 => {
                                let bit = 1u32 << r;
                                if (d_fp & bit) == 0 {
                                    u_fp |= bit;
                                }
                            }
                            MReg::V(v) if active_vwords > 0 => {
                                let lid = at(&vreg_to_local, to_idx(v.0), u32::MAX);
                                if lid != u32::MAX {
                                    let w = to_idx(lid >> 6);
                                    let bit = 1u64 << (lid & 63);
                                    if w < active_vwords && (at(&def_v, vbase + w, 0u64) & bit) == 0 {
                                        bitor_at(&mut up_v, vbase + w, bit);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    for &(dst, _) in copy_slice {
                        match dst {
                            MReg::P(PReg::Gpr(r)) if r <= 30 => d_gpr |= 1u32 << r,
                            MReg::P(PReg::V(r)) if r <= 31 => d_fp |= 1u32 << r,
                            MReg::V(v) if active_vwords > 0 => {
                                let lid = at(&vreg_to_local, to_idx(v.0), u32::MAX);
                                if lid != u32::MAX {
                                    let w = to_idx(lid >> 6);
                                    if w < active_vwords {
                                        bitor_at(&mut def_v, vbase + w, 1u64 << (lid & 63));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    continue;
                }

                let info = classify_inst(func, i, target);
                let ug = info.phys_gpr_uses & 0x7fff_ffff;
                let uf = info.phys_fp_uses;
                u_gpr |= ug & !d_gpr;
                u_fp |= uf & !d_fp;
                if active_vwords > 0 {
                    for &v in info.vreg_uses_slice() {
                        let lid = at(&vreg_to_local, to_idx(v), u32::MAX);
                        if lid != u32::MAX {
                            let w = to_idx(lid >> 6);
                            let bit = 1u64 << (lid & 63);
                            if w < active_vwords && (at(&def_v, vbase + w, 0u64) & bit) == 0 {
                                bitor_at(&mut up_v, vbase + w, bit);
                            }
                        }
                    }
                }
                d_gpr |= info.phys_gpr_defs & 0x7fff_ffff;
                d_fp |= info.phys_fp_defs;
                if active_vwords > 0 {
                    for &v in info.vreg_defs_slice() {
                        let lid = at(&vreg_to_local, to_idx(v), u32::MAX);
                        if lid != u32::MAX {
                            let w = to_idx(lid >> 6);
                            if w < active_vwords {
                                bitor_at(&mut def_v, vbase + w, 1u64 << (lid & 63));
                            }
                        }
                    }
                }
            }

            set(&mut up_gpr, slot, u_gpr);
            set(&mut up_fp, slot, u_fp);
            set(&mut def_gpr, slot, d_gpr);
            set(&mut def_fp, slot, d_fp);
        }

        // 3. Bounded backward liveness inside the natural loop body (`live_in[0]` = header `h` live-in).
        live_in_gpr.clear();
        live_in_gpr.extend_from_slice(&up_gpr);
        live_in_fp.clear();
        live_in_fp.extend_from_slice(&up_fp);
        live_in_v.clear();
        live_in_v.extend_from_slice(&up_v);

        if k_len > 1 {
            for _ in 0..MAX_LOOP_LIVENESS_ITERS {
                let mut changed = false;
                for slot in (0..k_len).rev() {
                    let b_idx = at(&loop_blocks, slot, 0usize);
                    let vbase = slot * active_vwords;
                    let mut out_gpr = 0u32;
                    let mut out_fp = 0u32;

                    for_each_block_succ(func, MBlock(u32_of(b_idx)), &order_pos, |succ| {
                        let s = to_idx(succ.0);
                        if s < n_blocks {
                            let s_slot = at(&block_to_slot, s, usize::MAX);
                            if s_slot != usize::MAX {
                                out_gpr |= at(&live_in_gpr, s_slot, 0u32);
                                out_fp |= at(&live_in_fp, s_slot, 0u32);
                                let svbase = s_slot * active_vwords;
                                for w in 0..active_vwords {
                                    let add = at(&live_in_v, svbase + w, 0u64)
                                        & !at(&def_v, vbase + w, 0u64);
                                    if let Some(cell) = live_in_v.get_mut(vbase + w) {
                                        let next = *cell | add;
                                        if next != *cell {
                                            *cell = next;
                                            changed = true;
                                        }
                                    }
                                }
                            }
                        }
                    });

                    let prev_g = at(&live_in_gpr, slot, 0u32);
                    let prev_f = at(&live_in_fp, slot, 0u32);
                    let new_g = prev_g | (out_gpr & !at(&def_gpr, slot, 0u32));
                    let new_f = prev_f | (out_fp & !at(&def_fp, slot, 0u32));
                    if new_g != prev_g || new_f != prev_f {
                        set(&mut live_in_gpr, slot, new_g);
                        set(&mut live_in_fp, slot, new_f);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
        }

        // 4. For each block in the loop body, mark instructions defining loop-carried registers
        //    (live-in to header `h` AND live into an in-loop successor) without further in-block
        //    non-terminator/non-branch-cmp consumers (including values fed into a latch `ParallelCopy`).
        let hdr_live_gpr = at(&live_in_gpr, 0, 0u32);
        let hdr_live_fp = at(&live_in_fp, 0, 0u32);
        for &b_idx in loop_blocks.iter() {
            needed.clear();
            consumed_later.clear();

            // Intersect union of in-loop successors' `live_in` with header `h`'s `live_in` (slot 0).
            for_each_block_succ(func, MBlock(u32_of(b_idx)), &order_pos, |succ| {
                let s = to_idx(succ.0);
                if s < n_blocks {
                    let s_slot = at(&block_to_slot, s, usize::MAX);
                    if s_slot != usize::MAX {
                        needed.gpr |= at(&live_in_gpr, s_slot, 0u32) & hdr_live_gpr;
                        needed.fp |= at(&live_in_fp, s_slot, 0u32) & hdr_live_fp;
                        let svbase = s_slot * active_vwords;
                        for w in 0..active_vwords {
                            let mut bits =
                                at(&live_in_v, svbase + w, 0u64) & at(&live_in_v, w, 0u64);
                            while bits != 0 {
                                let tz = bits.trailing_zeros() as usize;
                                bits &= bits - 1;
                                if let Some(&orig_v) = local_to_vreg.get(w * 64 + tz) {
                                    needed.insert_vreg(orig_v);
                                }
                            }
                        }
                    }
                }
            });

            let range = func.block_inst_range(MBlock(u32_of(b_idx)));
            let end = range.end.min(n_ops);
            if range.start >= end {
                continue;
            }

            // Identify trailing flag-setting compare feeding a conditional branch (e.g. `cmp i, n; b.lt loop`)
            // so a loop IV update feeding only that branch-compare still receives the recurrence boost.
            let mut branch_cmp_idx = usize::MAX;
            if end >= range.start + 2 && func.ops.get(end - 1).is_some_and(is_flag_cond_branch) {
                let mut scan = end - 1;
                while scan > range.start {
                    scan -= 1;
                    let Some(op) = func.ops.get(scan) else {
                        break;
                    };
                    if op.writes_flags() {
                        if matches!(
                            *op,
                            MOp::CmpImm { .. }
                                | MOp::CmpReg { .. }
                                | MOp::TstImm { .. }
                                | MOp::TstReg { .. }
                                | MOp::FpCmp { .. }
                        ) {
                            branch_cmp_idx = scan;
                        }
                        break;
                    }
                }
            }

            for i in (range.start..end).rev() {
                let Some(op) = func.ops.get(i) else {
                    continue;
                };
                if let MOp::ParallelCopy { start, len } = *op {
                    let cs = to_idx(start);
                    let ce = cs.saturating_add(usize::from(len)).min(func.copy_pairs.len());
                    copy_add_srcs.clear();
                    copy_kill_dsts.clear();
                    for &(dst, src) in func.copy_pairs.get(cs..ce).unwrap_or(&[]) {
                        if needed.contains_mreg(dst) && !consumed_later.contains_mreg(dst) {
                            copy_add_srcs.push(src);
                        }
                        copy_kill_dsts.push(dst);
                    }
                    for &dst in copy_kill_dsts.iter() {
                        needed.remove_mreg(dst);
                    }
                    for &src in copy_add_srcs.iter() {
                        needed.insert_mreg(src);
                    }
                    continue;
                }

                if op.is_unconditional_terminator() || op.is_conditional_branch() {
                    continue;
                }

                let info = classify_inst(func, i, target);
                let vdefs = info.vreg_defs_slice();
                let vuses = info.vreg_uses_slice();

                if !is_sched_barrier(func, i)
                    && needed.hits_unexcluded(&consumed_later, info.phys_gpr_defs, info.phys_fp_defs, vdefs)
                {
                    set(&mut is_recurrence, i, true);
                }

                // Kill defined registers from `needed` (earlier defs of the same reg are overwritten).
                needed.remove_info_mask(info.phys_gpr_defs, info.phys_fp_defs, vdefs);

                // Record uses as in-block consumers (except a trailing branch-feeding compare).
                if i != branch_cmp_idx {
                    consumed_later.insert_info_mask(info.phys_gpr_uses, info.phys_fp_uses, vuses);
                }
            }
        }

        // Clear `vreg_to_local` and `block_to_slot` entries for next header.
        for &v in local_to_vreg.iter() {
            set(&mut vreg_to_local, to_idx(v), u32::MAX);
        }
        local_to_vreg.clear();
        for &b_idx in loop_blocks.iter() {
            set(&mut block_to_slot, b_idx, usize::MAX);
        }
    }

    LoopAnalysis { is_recurrence, in_loop_block }
}

// -----------------------------------------------------------------------------
// 3b. Pre-RA SSA Liveness & Register-Pressure Tracker (`func.is_ssa == true`)
// -----------------------------------------------------------------------------

/// Per-function reusable scratch state tracking exact SSA `VReg` liveness and per-window
/// remaining-use counts (`rem_uses`) in strictly `O(N)` per-window cleanup time.
struct SsaLiveness<'a> {
    num_vregs: usize,
    num_vwords: usize,
    /// Bitmask of `VReg`s belonging to the GPR register bank (`Gpr32` / `Gpr64`).
    gpr_mask: Vec<u64, Alloc<'a>>,
    /// Bitmask of `VReg`s belonging to the FP/SIMD register bank (`Fp32` / `Fp64` / `Vec128`).
    fp_mask: Vec<u64, Alloc<'a>>,
    /// Flat per-block `live_out_v` bitsets (`n_blocks * num_vwords`), or empty if capped.
    block_live_out_v: Vec<u64, Alloc<'a>>,
    /// Current backward live `VReg` bitset while scanning a block from bottom to top (`num_vwords`).
    cur_live_v: Vec<u64, Alloc<'a>>,
    /// Remaining uses in the active window (+1 sentinel if live-out of window), indexed by `VReg.0`.
    rem_uses: Vec<u16, Alloc<'a>>,
    /// Currently-live flag for `VReg`s touched in the active window, indexed by `VReg.0`.
    vreg_live: Vec<bool, Alloc<'a>>,
}

impl<'a> SsaLiveness<'a> {
    /// Builds [`SsaLiveness`] for an SSA function, computing bounded cross-block `live_out_v`.
    fn new(func: &MFunc<'_>, target: TargetModel, alloc: Alloc<'a>) -> Option<Self> {
        if !func.is_ssa || func.vreg_class.is_empty() {
            return None;
        }
        let num_vregs = func.vreg_class.len();
        let num_vwords = num_vregs.div_ceil(64);
        let n_blocks = func.block_insts.len();

        let mut gpr_mask = vec_filled(num_vwords, 0u64, alloc);
        let mut fp_mask = vec_filled(num_vwords, 0u64, alloc);
        for (vi, &cls) in func.vreg_class.iter().enumerate() {
            let w = vi >> 6;
            let bit = 1u64 << (vi & 63);
            if cls.is_gpr() {
                bitor_at(&mut gpr_mask, w, bit);
            } else if cls.is_fp_or_vec() {
                bitor_at(&mut fp_mask, w, bit);
            }
        }

        let total_vwords = n_blocks.saturating_mul(num_vwords);
        let block_live_out_v = if total_vwords > 0
            && total_vwords <= MAX_SSA_LIVENESS_WORDS
            && !func.block_order.is_empty()
        {
            let mut order_pos = vec_filled(n_blocks, usize::MAX, alloc);
            for (pos, &blk) in func.block_order.iter().enumerate() {
                let b = to_idx(blk.0);
                if b < n_blocks && at(&order_pos, b, usize::MAX) == usize::MAX {
                    set(&mut order_pos, b, pos);
                }
            }

            let mut up_gpr = vec_filled(n_blocks, 0u32, alloc);
            let mut up_fp = vec_filled(n_blocks, 0u32, alloc);
            let mut up_v = vec_filled(total_vwords, 0u64, alloc);
            let mut def_gpr = vec_filled(n_blocks, 0u32, alloc);
            let mut def_fp = vec_filled(n_blocks, 0u32, alloc);
            let mut def_v = vec_filled(total_vwords, 0u64, alloc);

            for b in 0..n_blocks {
                compute_block_use_def(
                    func,
                    b,
                    b,
                    num_vwords,
                    target,
                    &mut up_gpr,
                    &mut up_fp,
                    &mut up_v,
                    &mut def_gpr,
                    &mut def_fp,
                    &mut def_v,
                );
            }

            let mut live_in_v = vec_filled(total_vwords, 0u64, alloc);
            let mut live_out_v = vec_filled(total_vwords, 0u64, alloc);
            live_in_v.copy_from_slice(&up_v);

            for _ in 0..MAX_LOOP_LIVENESS_ITERS {
                let mut changed = false;
                for &blk in func.block_order.iter().rev() {
                    let b = to_idx(blk.0);
                    if b >= n_blocks {
                        continue;
                    }
                    let vbase = b * num_vwords;
                    for_each_block_succ(func, blk, &order_pos, |succ| {
                        let s = to_idx(succ.0);
                        if s < n_blocks {
                            let svbase = s * num_vwords;
                            for w in 0..num_vwords {
                                let succ_in = at(&live_in_v, svbase + w, 0u64);
                                let out_prev = at(&live_out_v, vbase + w, 0u64);
                                let out_next = out_prev | succ_in;
                                if out_next != out_prev {
                                    set(&mut live_out_v, vbase + w, out_next);
                                    changed = true;
                                }
                                let in_prev = at(&live_in_v, vbase + w, 0u64);
                                let in_next = in_prev | (out_next & !at(&def_v, vbase + w, 0u64));
                                if in_next != in_prev {
                                    set(&mut live_in_v, vbase + w, in_next);
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
            live_out_v
        } else {
            Vec::new_in(alloc)
        };

        Some(Self {
            num_vregs,
            num_vwords,
            gpr_mask,
            fp_mask,
            block_live_out_v,
            cur_live_v: vec_filled(num_vwords, 0u64, alloc),
            rem_uses: vec_filled(num_vregs, 0u16, alloc),
            vreg_live: vec_filled(num_vregs, false, alloc),
        })
    }

    #[inline]
    fn cur_live_contains(&self, v: u32) -> bool {
        let w = to_idx(v >> 6);
        self.cur_live_v.get(w).is_some_and(|&bits| (bits & (1u64 << (v & 63))) != 0)
    }

    #[inline]
    fn cur_live_insert(&mut self, v: u32) {
        let w = to_idx(v >> 6);
        if let Some(slot) = self.cur_live_v.get_mut(w) {
            *slot |= 1u64 << (v & 63);
        }
    }

    #[inline]
    fn cur_live_remove(&mut self, v: u32) {
        let w = to_idx(v >> 6);
        if let Some(slot) = self.cur_live_v.get_mut(w) {
            *slot &= !(1u64 << (v & 63));
        }
    }

    /// Initializes `cur_live_v` to the `live_out_v` set at the bottom of block `b_idx`.
    fn init_block(&mut self, b_idx: usize) {
        let base = b_idx.saturating_mul(self.num_vwords);
        let end = base.saturating_add(self.num_vwords);
        if let Some(src) = self.block_live_out_v.get(base..end)
            && src.len() == self.cur_live_v.len()
        {
            self.cur_live_v.copy_from_slice(src);
        } else {
            for w in self.cur_live_v.iter_mut() {
                *w = 0;
            }
        }
    }

    /// Steps `cur_live_v` backward across a single instruction `func.ops[idx]`.
    fn step_backward_inst(&mut self, func: &MFunc<'_>, idx: usize, target: TargetModel) {
        if let Some(&MOp::ParallelCopy { start, len }) = func.ops.get(idx) {
            let cs = to_idx(start);
            let ce = cs.saturating_add(usize::from(len)).min(func.copy_pairs.len());
            let copy_slice = func.copy_pairs.get(cs..ce).unwrap_or(&[]);
            for &(dst, _) in copy_slice {
                if let MReg::V(v) = dst {
                    self.cur_live_remove(v.0);
                }
            }
            for &(_, src) in copy_slice {
                if let MReg::V(v) = src {
                    self.cur_live_insert(v.0);
                }
            }
            return;
        }
        let info = classify_inst(func, idx, target);
        for &v in info.vreg_defs_slice() {
            self.cur_live_remove(v);
        }
        for &v in info.vreg_uses_slice() {
            self.cur_live_insert(v);
        }
    }

    /// Counts currently live GPR and FP/SIMD `VReg`s in `cur_live_v`.
    #[inline]
    fn count_live_banks(&self) -> (u16, u16) {
        let mut gpr = 0u32;
        let mut fp = 0u32;
        for w in 0..self.num_vwords {
            let live = at(&self.cur_live_v, w, 0u64);
            gpr = gpr.saturating_add((live & at(&self.gpr_mask, w, 0u64)).count_ones());
            fp = fp.saturating_add((live & at(&self.fp_mask, w, 0u64)).count_ones());
        }
        (
            u16::try_from(gpr).unwrap_or(u16::MAX),
            u16::try_from(fp).unwrap_or(u16::MAX),
        )
    }

    /// Prepares per-window `rem_uses` and `vreg_live`, advances `cur_live_v` backward across
    /// `info[0..len]` to become `live_in` at `w_start`, and returns `(init_gpr_live, init_fp_live)`.
    fn enter_window(&mut self, info: &[InstInfo], len: usize) -> (u16, u16) {
        let window = info.get(..len).unwrap_or(&[]);

        // 1. Count intra-window remaining uses for every VReg used in the window.
        for ci in window {
            for &v in ci.vreg_uses_slice() {
                if let Some(slot) = self.rem_uses.get_mut(to_idx(v)) {
                    *slot = slot.saturating_add(1);
                }
            }
        }

        // 2. Add a +1 sentinel to `rem_uses[v]` for any window VReg still live after `w_end`
        //    so no intra-window consumer treats a live-out VReg as killed.
        for ci in window {
            for &v in ci.vreg_uses_slice() {
                let vi = to_idx(v);
                if vi < self.num_vregs && !at(&self.vreg_live, vi, true) && self.cur_live_contains(v) {
                    set(&mut self.vreg_live, vi, true);
                    if let Some(slot) = self.rem_uses.get_mut(vi) {
                        *slot = slot.saturating_add(1);
                    }
                }
            }
            for &v in ci.vreg_defs_slice() {
                let vi = to_idx(v);
                if vi < self.num_vregs && !at(&self.vreg_live, vi, true) && self.cur_live_contains(v) {
                    set(&mut self.vreg_live, vi, true);
                    if let Some(slot) = self.rem_uses.get_mut(vi) {
                        *slot = slot.saturating_add(1);
                    }
                }
            }
        }

        // 3. Step `cur_live_v` backward across `[w_start..w_end]` to produce exact `live_in` at `w_start`.
        for ci in window.iter().rev() {
            for &v in ci.vreg_defs_slice() {
                self.cur_live_remove(v);
            }
            for &v in ci.vreg_uses_slice() {
                self.cur_live_insert(v);
            }
        }

        // 4. Populate `vreg_live` at `w_start` for every VReg appearing in the window.
        for ci in window {
            for &v in ci.vreg_uses_slice() {
                let live = self.cur_live_contains(v);
                set(&mut self.vreg_live, to_idx(v), live);
            }
            for &v in ci.vreg_defs_slice() {
                let live = self.cur_live_contains(v);
                set(&mut self.vreg_live, to_idx(v), live);
            }
        }

        self.count_live_banks()
    }

    /// Cleans all per-window `rem_uses` and `vreg_live` entries touched by `info[0..len]` in `O(len)`.
    fn cleanup_window(&mut self, info: &[InstInfo], len: usize) {
        let window = info.get(..len).unwrap_or(&[]);
        for ci in window {
            for &v in ci.vreg_uses_slice() {
                let vi = to_idx(v);
                set(&mut self.rem_uses, vi, 0u16);
                set(&mut self.vreg_live, vi, false);
            }
            for &v in ci.vreg_defs_slice() {
                let vi = to_idx(v);
                set(&mut self.rem_uses, vi, 0u16);
                set(&mut self.vreg_live, vi, false);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// 4. Sliding-Window Scheduler: SSA Pressure Mode + Post-RA Bounded Beam-4
// -----------------------------------------------------------------------------

/// Immutable per-window DAG, heights, fusion context, and target pipe caps shared by greedy,
/// exact cycle simulation, and bounded Beam-4 lookahead search.
struct WindowCtx<'a> {
    ops: &'a [MOp],
    info: &'a [InstInfo; WINDOW_MAX],
    succs: &'a [u128; WINDOW_MAX],
    is_raw: &'a [u128; WINDOW_MAX],
    height: &'a [u16; WINDOW_MAX],
    pure_height: &'a [u16; WINDOW_MAX],
    rec_height: &'a [u16; WINDOW_MAX],
    pure_rec_height: &'a [u16; WINDOW_MAX],
    len: usize,
    is_ssa: bool,
    followed_by_flag_branch: bool,
    followed_by_cbz_rt: Option<MReg>,
    pipe_caps: [u8; 4],
    issue_width: u8,
    target: TargetModel,
}

/// Exact simulated execution cost of a complete window schedule (lexicographically minimized).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct ScheduleCost {
    /// Total simulated window makespan (`max(issue_cycle + lat)` over all instructions).
    makespan: u16,
    /// Exact completion cycle of loop-carried recurrences (`0` when no recurrences exist).
    rec_done: u16,
    /// Issue cycle of the final instruction in the window.
    last_issue_cycle: u16,
    /// `u8::MAX - fused_pairs` (lower means more macro-op fused pairs achieved).
    neg_fused: u8,
}

#[inline]
fn fuses_adrp_with_prev(ctx: &WindowCtx<'_>, prev_op_idx: u8, u: usize) -> bool {
    if ctx.is_ssa || !ctx.target.macro_fuse_adrp_add || prev_op_idx == NO_LOCAL_OP {
        return false;
    }
    let (Some(prev_op), Some(cur_op)) = (ctx.ops.get(usize::from(prev_op_idx)), ctx.ops.get(u)) else {
        return false;
    };
    is_adrp_fusion_pair(prev_op, cur_op)
}

#[inline]
fn effective_ready_at(
    ctx: &WindowCtx<'_>,
    prev_op_idx: u8,
    u: usize,
    raw_ready_at: u16,
    cur_cycle: u16,
) -> u16 {
    if fuses_adrp_with_prev(ctx, prev_op_idx, u) && raw_ready_at == cur_cycle.saturating_add(1) {
        cur_cycle
    } else {
        raw_ready_at
    }
}

/// Simulates exact Apple M5 / Graviton 5 issue-cycle makespan, loop-carried recurrence
/// completion cycle, and macro-op fusion count for a candidate topological `order[0..ctx.len]`.
fn simulate_schedule_cost(ctx: &WindowCtx<'_>, order: &[usize]) -> ScheduleCost {
    let mut ready_at = [0u16; WINDOW_MAX];
    let mut cycle = 0u16;
    let mut pipe_used = [0u8; 4];
    let mut issued_in_cycle = 0u8;
    let mut prev_op_idx = NO_LOCAL_OP;
    let mut max_completion = 0u16;
    let mut max_rec_done = 0u16;
    let mut fused_pairs = 0u8;

    for (step, &u) in order.iter().enumerate().take(ctx.len) {
        let fused_adrp = fuses_adrp_with_prev(ctx, prev_op_idx, u);
        let eff_ready = effective_ready_at(ctx, prev_op_idx, u, at(&ready_at, u, 0u16), cycle);
        let u_info = at(ctx.info, u, InstInfo::EMPTY);
        let p_idx = u_info.pipe.index();
        let extra_decode: u8 = if fused_adrp { 0 } else { 1 };
        let extra_pipe: u8 = if fused_adrp && matches!(ctx.ops.get(u), Some(MOp::AddLo12 { .. })) {
            0
        } else {
            1
        };

        if eff_ready > cycle {
            cycle = eff_ready;
            pipe_used = [0; 4];
            issued_in_cycle = 0;
        } else if issued_in_cycle.saturating_add(extra_decode) > ctx.issue_width
            || at(&pipe_used, p_idx, 0u8).saturating_add(extra_pipe) > at(&ctx.pipe_caps, p_idx, 1u8)
        {
            cycle = cycle.saturating_add(1);
            pipe_used = [0; 4];
            issued_in_cycle = 0;
        }

        add_at_u8(&mut pipe_used, p_idx, extra_pipe);
        issued_in_cycle = issued_in_cycle.saturating_add(extra_decode);
        if fused_adrp {
            fused_pairs = fused_pairs.saturating_add(1);
        }

        let u_lat = u16::from(u_info.lat);
        let done_c = cycle.saturating_add(u_lat);
        if done_c > max_completion {
            max_completion = done_c;
        }
        if at(ctx.rec_height, u, 0u16) > 0 && done_c > max_rec_done {
            max_rec_done = done_c;
        }

        if step + 1 == ctx.len && ctx.target.macro_fuse_cmp_br {
            let tail_fused = ctx.ops.get(u).is_some_and(|op| {
                (ctx.followed_by_flag_branch && is_cmp_br_fusible(op))
                    || (!ctx.is_ssa
                        && ctx.followed_by_cbz_rt.is_some_and(|rt| is_cbz_fusible(op, rt)))
            });
            if tail_fused {
                fused_pairs = fused_pairs.saturating_add(1);
            }
        }

        let mut smask = at(ctx.succs, u, 0u128);
        let raw_mask = at(ctx.is_raw, u, 0u128);
        while smask != 0 {
            let b = to_idx(smask.trailing_zeros());
            smask &= smask - 1;
            let edge_w = if ((raw_mask >> b) & 1) != 0 { u_lat } else { 0 };
            let avail = cycle.saturating_add(edge_w);
            max_at_u16(&mut ready_at, b, avail);
        }
        prev_op_idx = u8_of(u);
    }

    ScheduleCost {
        makespan: max_completion,
        rec_done: max_rec_done,
        last_issue_cycle: cycle,
        neg_fused: u8::MAX.saturating_sub(fused_pairs),
    }
}

/// Compact stack-only partial schedule state for bounded Beam-4 search (`MAX_BEAM_STATES = 4`).
#[derive(Clone, Copy)]
struct BeamState {
    ready: u128,
    scheduled: u128,
    pred_rem: [u8; BEAM_MAX_LEN],
    ready_at: [u16; BEAM_MAX_LEN],
    order: [u8; BEAM_MAX_LEN],
    cycle: u16,
    max_completion: u16,
    max_rec_done: u16,
    rem_pipe_ops: [u8; 4],
    pipe_used: [u8; 4],
    issued_in_cycle: u8,
    fused_pairs: u8,
    prev_op_idx: u8,
}

impl BeamState {
    const EMPTY: Self = Self {
        ready: 0,
        scheduled: 0,
        pred_rem: [0; BEAM_MAX_LEN],
        ready_at: [0; BEAM_MAX_LEN],
        order: [0; BEAM_MAX_LEN],
        cycle: 0,
        max_completion: 0,
        max_rec_done: 0,
        rem_pipe_ops: [0; 4],
        pipe_used: [0; 4],
        issued_in_cycle: 0,
        fused_pairs: 0,
        prev_op_idx: NO_LOCAL_OP,
    };
}

/// Intermediate beam-pruning rank using admissible pipe + critical-path lower bounds (lower = better).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct BeamRank {
    est_makespan: u16,
    est_rec_done: u16,
    cycle: u16,
    max_completion: u16,
    neg_fused: u8,
    last_op: u8,
}

impl BeamRank {
    const MAX: Self = Self {
        est_makespan: u16::MAX,
        est_rec_done: u16::MAX,
        cycle: u16::MAX,
        max_completion: u16::MAX,
        neg_fused: u8::MAX,
        last_op: u8::MAX,
    };
}

/// Selects up to `MAX_BEAM_BRANCH` diverse ready candidates from `state`, gluing `adrp` fusion
/// pairs immediately and combining greedy, pure-makespan, 2-step lookahead, and pipe-diverse picks.
fn select_beam_candidates(
    ctx: &WindowCtx<'_>,
    state: &BeamState,
) -> ([u8; MAX_BEAM_BRANCH], usize) {
    let mut out = [0u8; MAX_BEAM_BRANCH];

    // If the previous op was `adrp` and its fusible `addlo12`/`ldr` consumer is ready, glue it!
    if !ctx.is_ssa && ctx.target.macro_fuse_adrp_add && state.prev_op_idx != NO_LOCAL_OP {
        let mut scan = state.ready;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            if fuses_adrp_with_prev(ctx, state.prev_op_idx, u) {
                set(&mut out, 0, u8_of(u));
                return (out, 1);
            }
        }
    }

    // When `<= MAX_BEAM_BRANCH` instructions are ready, expand all of them exhaustively.
    if state.ready.count_ones() <= u32_of(MAX_BEAM_BRANCH) {
        let mut count = 0usize;
        let mut scan = state.ready;
        while scan != 0 && count < MAX_BEAM_BRANCH {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            set(&mut out, count, u8_of(u));
            count += 1;
        }
        return (out, count);
    }

    // Compute the effective issue cycle if we were to advance from `state.cycle`.
    let mut eff_cycle = state.cycle;
    let mut eff_pipe_used = state.pipe_used;
    let eff_issued = state.issued_in_cycle;
    if eff_issued >= ctx.issue_width {
        eff_cycle = eff_cycle.saturating_add(1);
        eff_pipe_used = [0; 4];
    } else {
        let mut min_ready = u16::MAX;
        let mut has_ready_free = false;
        let mut scan = state.ready;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            let ra = at(&state.ready_at, u, 0u16);
            if ra < min_ready {
                min_ready = ra;
            }
            if ra <= eff_cycle {
                let p = at(ctx.info, u, InstInfo::EMPTY).pipe.index();
                if at(&eff_pipe_used, p, 0u8) < at(&ctx.pipe_caps, p, 1u8) {
                    has_ready_free = true;
                }
            }
        }
        if min_ready > eff_cycle {
            eff_cycle = min_ready;
            eff_pipe_used = [0; 4];
        } else if !has_ready_free && eff_issued > 0 {
            eff_cycle = eff_cycle.saturating_add(1);
            eff_pipe_used = [0; 4];
        }
    }

    let mut picked = 0u128;
    let mut count = 0usize;
    let mut first_pipe = usize::MAX;

    // Candidate 0: Greedy boosted-height + pipe-balanced pick.
    {
        let mut best_u = usize::MAX;
        let mut best_key: (bool, bool, u16, u8, bool, u16, usize) = (false, false, 0, 0, false, 0, 0);
        let mut scan = state.ready;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            let u_info = at(ctx.info, u, InstInfo::EMPTY);
            let p = u_info.pipe.index();
            let used = at(&eff_pipe_used, p, 0u8);
            let cap = at(&ctx.pipe_caps, p, 1u8);
            let ra = at(&state.ready_at, u, 0u16);
            let key = (
                ra <= eff_cycle,
                used < cap,
                at(ctx.height, u, 0u16),
                cap.saturating_sub(used),
                matches!(u_info.mem, MemKind::Load),
                u16::MAX.saturating_sub(ra),
                usize::MAX - u,
            );
            if best_u == usize::MAX || key > best_key {
                best_u = u;
                best_key = key;
            }
        }
        if best_u != usize::MAX {
            set(&mut out, count, u8_of(best_u));
            count += 1;
            picked |= 1u128 << best_u;
            first_pipe = at(ctx.info, best_u, InstInfo::EMPTY).pipe.index();
        }
    }

    // Candidate 1: Pure critical-path height (prevents `rec_boost` from delaying a longer chain).
    {
        let mut best_u = usize::MAX;
        let mut best_key: (bool, bool, u16, u16, usize) = (false, false, 0, 0, 0);
        let mut scan = state.ready & !picked;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            let p = at(ctx.info, u, InstInfo::EMPTY).pipe.index();
            let ra = at(&state.ready_at, u, 0u16);
            let key = (
                ra <= eff_cycle,
                at(&eff_pipe_used, p, 0u8) < at(&ctx.pipe_caps, p, 1u8),
                at(ctx.pure_height, u, 0u16),
                u16::MAX.saturating_sub(ra),
                usize::MAX - u,
            );
            if best_u == usize::MAX || key > best_key {
                best_u = u;
                best_key = key;
            }
        }
        if best_u != usize::MAX && count < MAX_BEAM_BRANCH {
            set(&mut out, count, u8_of(best_u));
            count += 1;
            picked |= 1u128 << best_u;
        }
    }

    // Candidate 2: 2-step lookahead — prioritizes `u` that immediately unlocks the highest-height successor.
    {
        let mut best_u = usize::MAX;
        let mut best_key: (bool, bool, u16, u16, u16, usize) = (false, false, 0, 0, 0, 0);
        let mut scan = state.ready & !picked;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            let mut unlock_h = 0u16;
            let mut smask = at(ctx.succs, u, 0u128);
            while smask != 0 {
                let b = to_idx(smask.trailing_zeros());
                smask &= smask - 1;
                let bh = at(ctx.height, b, 0u16);
                if at(&state.pred_rem, b, 0u8) == 1 && bh > unlock_h {
                    unlock_h = bh;
                }
            }
            let p = at(ctx.info, u, InstInfo::EMPTY).pipe.index();
            let key = (
                at(&state.ready_at, u, 0u16) <= eff_cycle,
                at(&eff_pipe_used, p, 0u8) < at(&ctx.pipe_caps, p, 1u8),
                unlock_h,
                at(ctx.rec_height, u, 0u16),
                at(ctx.pure_height, u, 0u16),
                usize::MAX - u,
            );
            if best_u == usize::MAX || key > best_key {
                best_u = u;
                best_key = key;
            }
        }
        if best_u != usize::MAX && count < MAX_BEAM_BRANCH {
            set(&mut out, count, u8_of(best_u));
            count += 1;
            picked |= 1u128 << best_u;
        }
    }

    // Candidate 3: Pipe-diverse pick (prefers a different execution pipe from Candidate 0).
    {
        let mut best_u = usize::MAX;
        let mut best_key: (bool, bool, bool, u8, u16, usize) = (false, false, false, 0, 0, 0);
        let mut scan = state.ready & !picked;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            let p = at(ctx.info, u, InstInfo::EMPTY).pipe.index();
            let used = at(&eff_pipe_used, p, 0u8);
            let cap = at(&ctx.pipe_caps, p, 1u8);
            let key = (
                at(&state.ready_at, u, 0u16) <= eff_cycle,
                used < cap,
                p != first_pipe,
                cap.saturating_sub(used),
                at(ctx.height, u, 0u16),
                usize::MAX - u,
            );
            if best_u == usize::MAX || key > best_key {
                best_u = u;
                best_key = key;
            }
        }
        if best_u != usize::MAX && count < MAX_BEAM_BRANCH {
            set(&mut out, count, u8_of(best_u));
            count += 1;
        }
    }

    (out, count)
}

/// Advances a [`BeamState`] by issuing instruction `u` at position `step`, using the exact same
/// per-pipe dispatch and macro-op fusion timing model as [`simulate_schedule_cost`].
fn advance_beam_state(ctx: &WindowCtx<'_>, mut state: BeamState, u: usize, step: usize) -> BeamState {
    let fused_adrp = fuses_adrp_with_prev(ctx, state.prev_op_idx, u);
    let eff_ready =
        effective_ready_at(ctx, state.prev_op_idx, u, at(&state.ready_at, u, 0u16), state.cycle);
    let u_info = at(ctx.info, u, InstInfo::EMPTY);
    let p_idx = u_info.pipe.index();
    let extra_decode: u8 = if fused_adrp { 0 } else { 1 };
    let extra_pipe: u8 = if fused_adrp && matches!(ctx.ops.get(u), Some(MOp::AddLo12 { .. })) {
        0
    } else {
        1
    };

    if eff_ready > state.cycle {
        state.cycle = eff_ready;
        state.pipe_used = [0; 4];
        state.issued_in_cycle = 0;
    } else if state.issued_in_cycle.saturating_add(extra_decode) > ctx.issue_width
        || at(&state.pipe_used, p_idx, 0u8).saturating_add(extra_pipe)
            > at(&ctx.pipe_caps, p_idx, 1u8)
    {
        state.cycle = state.cycle.saturating_add(1);
        state.pipe_used = [0; 4];
        state.issued_in_cycle = 0;
    }

    add_at_u8(&mut state.pipe_used, p_idx, extra_pipe);
    state.issued_in_cycle = state.issued_in_cycle.saturating_add(extra_decode);
    if let Some(slot) = state.rem_pipe_ops.get_mut(p_idx) {
        *slot = slot.saturating_sub(1);
    }
    if fused_adrp {
        state.fused_pairs = state.fused_pairs.saturating_add(1);
    }

    let u_lat = u16::from(u_info.lat);
    let done_c = state.cycle.saturating_add(u_lat);
    if done_c > state.max_completion {
        state.max_completion = done_c;
    }
    if at(ctx.rec_height, u, 0u16) > 0 && done_c > state.max_rec_done {
        state.max_rec_done = done_c;
    }

    if step + 1 == ctx.len && ctx.target.macro_fuse_cmp_br {
        let tail_fused = ctx.ops.get(u).is_some_and(|op| {
            (ctx.followed_by_flag_branch && is_cmp_br_fusible(op))
                || (!ctx.is_ssa
                    && ctx.followed_by_cbz_rt.is_some_and(|rt| is_cbz_fusible(op, rt)))
        });
        if tail_fused {
            state.fused_pairs = state.fused_pairs.saturating_add(1);
        }
    }

    set(&mut state.order, step, u8_of(u));
    let u_bit = 1u128 << u;
    state.ready &= !u_bit;
    state.scheduled |= u_bit;
    state.prev_op_idx = u8_of(u);

    let mut smask = at(ctx.succs, u, 0u128);
    let raw_mask = at(ctx.is_raw, u, 0u128);
    while smask != 0 {
        let b = to_idx(smask.trailing_zeros());
        smask &= smask - 1;
        let edge_w = if ((raw_mask >> b) & 1) != 0 { u_lat } else { 0 };
        let avail = state.cycle.saturating_add(edge_w);
        max_at_u16(&mut state.ready_at, b, avail);
        if let Some(slot) = state.pred_rem.get_mut(b) {
            *slot = slot.saturating_sub(1);
            if *slot == 0 {
                state.ready |= 1u128 << b;
            }
        }
    }

    state
}

/// Computes an admissible lower-bound [`BeamRank`] combining critical-path latency of ready
/// instructions and per-pipe throughput capacity of all remaining unscheduled instructions.
fn compute_beam_rank(ctx: &WindowCtx<'_>, state: &BeamState, last_u: usize) -> BeamRank {
    let mut est_ms = state.max_completion;
    let mut est_rec = state.max_rec_done;

    // 1. Critical-path lower bound from all currently ready instructions.
    let mut scan = state.ready;
    while scan != 0 {
        let r = to_idx(scan.trailing_zeros());
        scan &= scan - 1;
        let eff_r =
            effective_ready_at(ctx, state.prev_op_idx, r, at(&state.ready_at, r, 0u16), state.cycle);
        let mut e = if eff_r > state.cycle { eff_r } else { state.cycle };
        if e == state.cycle && !fuses_adrp_with_prev(ctx, state.prev_op_idx, r) {
            let p = at(ctx.info, r, InstInfo::EMPTY).pipe.index();
            if state.issued_in_cycle >= ctx.issue_width
                || at(&state.pipe_used, p, 0u8) >= at(&ctx.pipe_caps, p, 1u8)
            {
                e = e.saturating_add(1);
            }
        }
        let ms_lb = e.saturating_add(at(ctx.pure_height, r, 0u16));
        if ms_lb > est_ms {
            est_ms = ms_lb;
        }
        let prh = at(ctx.pure_rec_height, r, 0u16);
        if prh > 0 {
            let rc_lb = e.saturating_add(prh);
            if rc_lb > est_rec {
                est_rec = rc_lb;
            }
        }
    }

    // 2. Per-pipe remaining throughput lower bound.
    let cycle_rem = ctx.issue_width.saturating_sub(state.issued_in_cycle);
    for p in 0..4 {
        let rem = at(&state.rem_pipe_ops, p, 0u8);
        if rem == 0 {
            continue;
        }
        let cap = at(&ctx.pipe_caps, p, 1u8);
        let pipe_rem = cap.saturating_sub(at(&state.pipe_used, p, 0u8));
        let avail_now = if cycle_rem < pipe_rem { cycle_rem } else { pipe_rem };
        let pipe_lb = if rem <= avail_now {
            state.cycle.saturating_add(1)
        } else {
            let after_now = rem - avail_now;
            let extra_cycles = u16::from(after_now.div_ceil(cap));
            state.cycle.saturating_add(extra_cycles).saturating_add(1)
        };
        if pipe_lb > est_ms {
            est_ms = pipe_lb;
        }
    }

    BeamRank {
        est_makespan: est_ms,
        est_rec_done: est_rec,
        cycle: state.cycle,
        max_completion: state.max_completion,
        neg_fused: u8::MAX.saturating_sub(state.fused_pairs),
        last_op: u8_of(last_u),
    }
}

/// Strictly bounded, deterministic, stack-only Beam-4 schedule search (`MAX_BEAM_STATES = 4`) for
/// post-RA hot inner loop windows (`4..=96` instructions).
fn beam_search_window(
    ctx: &WindowCtx<'_>,
    init_pred_rem: &[u8; WINDOW_MAX],
) -> Option<([usize; WINDOW_MAX], ScheduleCost)> {
    if ctx.is_ssa || ctx.len > BEAM_MAX_LEN {
        return None;
    }
    let mut init = BeamState::EMPTY;
    for u in 0..ctx.len {
        let pr = at(init_pred_rem, u, 0u8);
        set(&mut init.pred_rem, u, pr);
        if pr == 0 {
            init.ready |= 1u128 << u;
        }
        let p = at(ctx.info, u, InstInfo::EMPTY).pipe.index();
        add_at_u8(&mut init.rem_pipe_ops, p, 1);
    }

    let mut beam = [BeamState::EMPTY; MAX_BEAM_STATES];
    set(&mut beam, 0, init);
    let mut beam_len = 1usize;

    for step in 0..ctx.len {
        let mut next_beam = [BeamState::EMPTY; MAX_BEAM_STATES];
        let mut next_rank = [BeamRank::MAX; MAX_BEAM_STATES];
        let mut next_len = 0usize;

        for &state in beam.get(..beam_len).unwrap_or(&[]) {
            if state.ready == 0 {
                continue;
            }
            let (cands, cand_len) = select_beam_candidates(ctx, &state);
            for &cand_u in cands.get(..cand_len).unwrap_or(&[]) {
                let u = usize::from(cand_u);
                let cand_state = advance_beam_state(ctx, state, u, step);
                let cand_rank = compute_beam_rank(ctx, &cand_state, u);

                // Deduplicate identical `scheduled` subsets so all 4 beam slots stay distinct.
                let mut dup_idx = usize::MAX;
                for (k, nb) in next_beam.iter().enumerate().take(next_len) {
                    if nb.scheduled == cand_state.scheduled {
                        dup_idx = k;
                        break;
                    }
                }
                if dup_idx != usize::MAX {
                    if cand_rank < at(&next_rank, dup_idx, BeamRank::MAX) {
                        set(&mut next_beam, dup_idx, cand_state);
                        set(&mut next_rank, dup_idx, cand_rank);
                    }
                } else if next_len < MAX_BEAM_STATES {
                    set(&mut next_beam, next_len, cand_state);
                    set(&mut next_rank, next_len, cand_rank);
                    next_len += 1;
                } else {
                    let mut worst_k = 0usize;
                    let mut worst_r = at(&next_rank, 0, BeamRank::MAX);
                    for (k, &rk) in next_rank.iter().enumerate().skip(1) {
                        if rk > worst_r {
                            worst_k = k;
                            worst_r = rk;
                        }
                    }
                    if cand_rank < worst_r {
                        set(&mut next_beam, worst_k, cand_state);
                        set(&mut next_rank, worst_k, cand_rank);
                    }
                }
            }
        }

        if next_len == 0 {
            return None;
        }

        // Deterministic insertion sort over `<= 4` beam slots by `BeamRank`.
        for i in 1..next_len {
            let mut j = i;
            while j > 0 && at(&next_rank, j, BeamRank::MAX) < at(&next_rank, j - 1, BeamRank::MAX) {
                next_rank.swap(j, j - 1);
                next_beam.swap(j, j - 1);
                j -= 1;
            }
        }

        beam = next_beam;
        beam_len = next_len;
    }

    let mut best_order = [0usize; WINDOW_MAX];
    let mut best_cost: Option<ScheduleCost> = None;

    for state in beam.iter().take(beam_len) {
        let mut ord = [0usize; WINDOW_MAX];
        for (i, &op_u8) in state.order.iter().enumerate().take(ctx.len) {
            set(&mut ord, i, usize::from(op_u8));
        }
        let cost = simulate_schedule_cost(ctx, ord.get(..ctx.len).unwrap_or(&[]));
        if best_cost.is_none_or(|bc| cost < bc) {
            best_cost = Some(cost);
            best_order = ord;
        }
    }

    best_cost.map(|c| (best_order, c))
}

/// Schedules one barrier-free window `func[w_start..w_end]` (`2 <= len <= WINDOW_MAX`) in place,
/// preserving exact RAW/WAR/WAW + memory + single-live-`VFlag` NZCV serialization, pre-RA SSA
/// register-pressure control, post-RA bounded Beam-4 lookahead on hot loops, and Apple Silicon /
/// Graviton 5 macro-op fusion (`cmp`/`tst`/`adds`/`subs`/`ands`+`b.cc`, `adrp`+`addlo12`/`ldr`,
/// adjacent `and`/`ubfm`+`cbz`/`cbnz`).
#[allow(clippy::too_many_arguments)]
fn schedule_window(
    func: &mut MFunc<'_>,
    w_start: usize,
    w_end: usize,
    followed_by_flag_branch: bool,
    followed_by_cbz_rt: Option<MReg>,
    in_loop: bool,
    is_recurrence: &[bool],
    mut ssa_live: Option<&mut SsaLiveness<'_>>,
    target: TargetModel,
    stats: &mut MSchedStats,
) {
    let len = w_end - w_start;
    if len < 2 || len > WINDOW_MAX {
        return;
    }

    let mut info = [InstInfo::EMPTY; WINDOW_MAX];
    let mut rec_boost = [0u16; WINDOW_MAX];
    let mut last_flag_writer: Option<usize> = None;
    let mut has_any_recurrence = false;

    for a in 0..len {
        let ga = w_start + a;
        let ci = classify_inst(func, ga, target);
        set(&mut info, a, ci);
        if ci.writes_flags {
            last_flag_writer = Some(a);
        }
        if at(is_recurrence, ga, false) {
            set(&mut rec_boost, a, RECURRENCE_HEIGHT_BOOST);
            has_any_recurrence = true;
        }
    }

    // 1. Build exact intra-window RAW / WAR / WAW + memory + NZCV VFlag + trap-order DAG.
    let mut succs = [0u128; WINDOW_MAX];
    let mut is_raw = [0u128; WINDOW_MAX];
    let mut pred_rem = [0u8; WINDOW_MAX];

    for a in 0..len {
        let ia = at(&info, a, InstInfo::EMPTY);
        let a_vdefs = ia.vreg_defs_slice();
        let a_vuses = ia.vreg_uses_slice();

        for b in (a + 1)..len {
            let ib = at(&info, b, InstInfo::EMPTY);
            let b_vdefs = ib.vreg_defs_slice();
            let b_vuses = ib.vreg_uses_slice();

            // Exact RAW / WAR / WAW across physical GPRs, physical FPs, VRegs, and NZCV/VFlags.
            // Serializing all flag writers (`WAW`) and bracketing flag readers (`RAW` + `WAR`)
            // guarantees two live `VFlag` chains never interleave (`mverify` single-live-`VFlag`).
            let raw = (ia.phys_gpr_defs & ib.phys_gpr_uses) != 0
                || (ia.phys_fp_defs & ib.phys_fp_uses) != 0
                || vreg_slices_intersect(a_vdefs, b_vuses)
                || (ia.writes_flags && ib.reads_flags);
            let war = (ia.phys_gpr_uses & ib.phys_gpr_defs) != 0
                || (ia.phys_fp_uses & ib.phys_fp_defs) != 0
                || vreg_slices_intersect(a_vuses, b_vdefs)
                || (ia.reads_flags && ib.writes_flags);
            let waw = (ia.phys_gpr_defs & ib.phys_gpr_defs) != 0
                || (ia.phys_fp_defs & ib.phys_fp_defs) != 0
                || vreg_slices_intersect(a_vdefs, b_vdefs)
                || (ia.writes_flags && ib.writes_flags);

            // Loads reorder freely past loads; stores order against loads and stores.
            let mem_dep = !matches!(
                (ia.mem, ib.mem),
                (MemKind::None, _) | (_, MemKind::None) | (MemKind::Load, MemKind::Load)
            );

            // Preserve exact trap ordering relative to stores and other potentially trapping ops.
            let trap_dep = (ia.may_trap && ib.may_trap)
                || (ia.may_trap && matches!(ib.mem, MemKind::Store))
                || (matches!(ia.mem, MemKind::Store) && ib.may_trap);

            if raw || war || waw || mem_dep || trap_dep {
                let b_bit = 1u128 << b;
                bitor_at(&mut succs, a, b_bit);
                if raw {
                    bitor_at(&mut is_raw, a, b_bit);
                }
                add_at_u8(&mut pred_rem, b, 1);
            }
        }
    }

    // 2. Compute critical-path `height`, `pure_height`, and RAW-propagated `rec_height` /
    //    `pure_rec_height` backwards BEFORE adding synthetic macro-op fusion lock edges, so
    //    synthetic lock edges never distort critical-path heights.
    let mut height = [0u16; WINDOW_MAX];
    let mut pure_height = [0u16; WINDOW_MAX];
    let mut pure_rec_height = [0u16; WINDOW_MAX];
    let mut rec_height = [0u16; WINDOW_MAX];
    for a in (0..len).rev() {
        let a_lat = u16::from(at(&info, a, InstInfo::EMPTY).lat);
        let rb = at(&rec_boost, a, 0u16);
        let mut h = a_lat;
        let mut ph = a_lat;
        let mut prh = if rb > 0 { a_lat } else { 0 };
        let mut mask = at(&succs, a, 0u128);
        let raw_mask = at(&is_raw, a, 0u128);
        while mask != 0 {
            let b = to_idx(mask.trailing_zeros());
            mask &= mask - 1;
            let is_raw_edge = ((raw_mask >> b) & 1) != 0;
            let edge_w = if is_raw_edge { a_lat } else { 0 };
            let cand = edge_w.saturating_add(at(&height, b, 0u16));
            if cand > h {
                h = cand;
            }
            let p_cand = edge_w.saturating_add(at(&pure_height, b, 0u16));
            if p_cand > ph {
                ph = p_cand;
            }
            let b_prh = at(&pure_rec_height, b, 0u16);
            if is_raw_edge && b_prh > 0 {
                let r_cand = a_lat.saturating_add(b_prh);
                if r_cand > prh {
                    prh = r_cand;
                }
            }
        }
        set(&mut height, a, h.saturating_add(rb));
        set(&mut pure_height, a, ph);
        set(&mut pure_rec_height, a, prh);
        set(
            &mut rec_height,
            a,
            if prh > 0 { prh.saturating_add(RECURRENCE_HEIGHT_BOOST) } else { 0 },
        );
    }

    // 3a. Post-RA `Adrp` + (`AddLo12` | `Ldr { BaseOff }`) Fusion Lock: for adjacent pairs
    //     `(a, b = a + 1)`, ensure every earlier predecessor `p < a` of `b` precedes `a` too
    //     (strictly forward edges `p < a`, guaranteed acyclic) so `b` becomes ready immediately
    //     when `a` is scheduled and stays glued to `a`.
    if !func.is_ssa && target.macro_fuse_adrp_add {
        for a in 0..len.saturating_sub(1) {
            let b = a + 1;
            if let (Some(op_a), Some(op_b)) = (func.ops.get(w_start + a), func.ops.get(w_start + b))
                && is_adrp_fusion_pair(op_a, op_b)
            {
                let a_bit = 1u128 << a;
                let b_bit = 1u128 << b;
                for p in 0..a {
                    let sp = at(&succs, p, 0u128);
                    if (sp & b_bit) != 0 && (sp & a_bit) == 0 {
                        bitor_at(&mut succs, p, a_bit);
                        add_at_u8(&mut pred_rem, a, 1);
                    }
                }
            }
        }
    }

    // 3b. Branch Macro-Op Fusion Locks:
    //     - `cmp`/`tst`/`adds`/`subs`/`ands` locked to final slot before `b.cc` (when `succs[f] == 0`),
    //     - post-RA adjacent `and`/`ubfm` locked to final slot before `cbz`/`cbnz` (when `succs[last] == 0`).
    if target.macro_fuse_cmp_br
        && followed_by_flag_branch
        && let Some(f) = last_flag_writer
        && func.ops.get(w_start + f).is_some_and(is_cmp_br_fusible)
        && at(&succs, f, 0u128) == 0
    {
        let f_bit = 1u128 << f;
        for u in 0..len {
            if u == f {
                continue;
            }
            if (at(&succs, u, 0u128) & f_bit) == 0 {
                bitor_at(&mut succs, u, f_bit);
                add_at_u8(&mut pred_rem, f, 1);
            }
        }
    } else if !func.is_ssa
        && target.macro_fuse_cmp_br
        && let Some(rt) = followed_by_cbz_rt
    {
        let last = len - 1;
        if func.ops.get(w_start + last).is_some_and(|op| is_cbz_fusible(op, rt))
            && at(&succs, last, 0u128) == 0
        {
            let last_bit = 1u128 << last;
            for u in 0..last {
                if (at(&succs, u, 0u128) & last_bit) == 0 {
                    bitor_at(&mut succs, u, last_bit);
                    add_at_u8(&mut pred_rem, last, 1);
                }
            }
        }
    }

    // 3c. Pre-RA SSA induction-pointer tie-breaker: for each pointer-advance op (`AddImm`/`SubImm`/
    //     `AddReg`/`SubReg`/`AddExt`/`SubExt`) defining a new `VReg` from `base`, record:
    //     - `ptr_step_mem_peers`: all `Ldr`/`Str`/`Ldp`/`Stp` ops in this window reading `base` as
    //       their address base (deferred while any such peer is ready), and
    //     - `ptr_step_store_peers`: the `Str`/`Stp` subset reading `base` as their address base
    //       (deferred while any such store is still unscheduled, so `str [base]` always issues
    //       before `base_next = base + off` and `base` dies right at the step for `mra` coalescing).
    let mut ptr_step_mem_peers = [0u128; WINDOW_MAX];
    let mut ptr_step_store_peers = [0u128; WINDOW_MAX];
    if func.is_ssa {
        let mut mem_base = [None; WINDOW_MAX];
        let mut mem_mask = 0u128;
        let mut store_mask = 0u128;
        for j in 0..len {
            if let Some(op) = func.ops.get(w_start + j)
                && let Some(base) = mem_base_vreg(op)
            {
                set(&mut mem_base, j, Some(base));
                mem_mask |= 1u128 << j;
                if matches!(*op, MOp::Str { .. } | MOp::Stp { .. }) {
                    store_mask |= 1u128 << j;
                }
            }
        }
        if mem_mask != 0 {
            for i in 0..len {
                if let Some(op) = func.ops.get(w_start + i) {
                    let (b0, b1) = ssa_ptr_step_bases(op);
                    if b0.is_none() && b1.is_none() {
                        continue;
                    }
                    let succ_i = at(&succs, i, 0u128);
                    let mut peers = 0u128;
                    let mut scan = mem_mask & !(1u128 << i);
                    while scan != 0 {
                        let j = to_idx(scan.trailing_zeros());
                        scan &= scan - 1;
                        let mb = at(&mem_base, j, None);
                        let matches_base = (b0.is_some() && mb == b0) || (b1.is_some() && mb == b1);
                        if ((succ_i >> j) & 1) == 0 && matches_base {
                            peers |= 1u128 << j;
                        }
                    }
                    if peers != 0 {
                        set(&mut ptr_step_mem_peers, i, peers);
                        set(&mut ptr_step_store_peers, i, peers & store_mask);
                    }
                }
            }
        }
    }

    // 4. Pipe-balanced critical-path list scheduling (with SSA register-pressure control when
    //    `func.is_ssa == true`, and post-RA Beam-4 comparison on hot loops).
    let pipe_caps = [
        target.alu_pipes.max(1),
        target.ldst_pipes.max(1),
        target.fp_pipes.max(1),
        target.mul_pipes.max(1),
    ];
    let issue_width = target.decode_width.max(1);
    let init_pred_rem = pred_rem;

    let ctx = WindowCtx {
        ops: func.ops.get(w_start..w_end).unwrap_or(&[]),
        info: &info,
        succs: &succs,
        is_raw: &is_raw,
        height: &height,
        pure_height: &pure_height,
        rec_height: &rec_height,
        pure_rec_height: &pure_rec_height,
        len,
        is_ssa: func.is_ssa,
        followed_by_flag_branch,
        followed_by_cbz_rt,
        pipe_caps,
        issue_width,
        target,
    };

    let (mut cur_gpr_live, mut cur_fp_live) = if let Some(ssa) = ssa_live.as_deref_mut() {
        ssa.enter_window(&info, len)
    } else {
        (0u16, 0u16)
    };

    let mut ready = 0u128;
    for (u, &pr) in pred_rem.iter().enumerate().take(len) {
        if pr == 0 {
            ready |= 1u128 << u;
        }
    }
    let mut unscheduled: u128 = if len >= 128 { u128::MAX } else { (1u128 << len) - 1 };

    let mut ready_at = [0u16; WINDOW_MAX];
    let mut order = [0usize; WINDOW_MAX];
    let mut cycle = 0u16;
    let mut pipe_used = [0u8; 4];
    let mut issued_in_cycle = 0u8;
    let mut prev_op_idx = NO_LOCAL_OP;

    for step in 0..len {
        if ready == 0 {
            // Defensive: DAG is acyclic; clean SSA state and abort window if unexpectedly empty.
            if let Some(ssa) = ssa_live.as_deref_mut() {
                ssa.cleanup_window(&info, len);
            }
            return;
        }

        // Check if an `adrp` fusion partner is ready immediately after `prev_op_idx`.
        let mut has_adrp_fuse_cand = false;
        if !ctx.is_ssa && target.macro_fuse_adrp_add && prev_op_idx != NO_LOCAL_OP {
            let mut fscan = ready;
            while fscan != 0 {
                let u = to_idx(fscan.trailing_zeros());
                fscan &= fscan - 1;
                if fuses_adrp_with_prev(&ctx, prev_op_idx, u) {
                    has_adrp_fuse_cand = true;
                    break;
                }
            }
        }

        if !has_adrp_fuse_cand {
            if issued_in_cycle >= issue_width {
                cycle = cycle.saturating_add(1);
                pipe_used = [0; 4];
                issued_in_cycle = 0;
            }

            // Advance `cycle` when no ready instruction is available yet or all ready-now pipes are full.
            let mut min_ready = u16::MAX;
            let mut has_ready_free_pipe = false;
            let mut scan = ready;
            while scan != 0 {
                let u = to_idx(scan.trailing_zeros());
                scan &= scan - 1;
                let ra = effective_ready_at(&ctx, prev_op_idx, u, at(&ready_at, u, 0u16), cycle);
                if ra < min_ready {
                    min_ready = ra;
                }
                if ra <= cycle {
                    let p_idx = at(&info, u, InstInfo::EMPTY).pipe.index();
                    if at(&pipe_used, p_idx, 0u8) < at(&pipe_caps, p_idx, 1u8) {
                        has_ready_free_pipe = true;
                    }
                }
            }
            if min_ready > cycle {
                cycle = min_ready;
                pipe_used = [0; 4];
                issued_in_cycle = 0;
            } else if !has_ready_free_pipe && issued_in_cycle > 0 {
                cycle = cycle.saturating_add(1);
                pipe_used = [0; 4];
                issued_in_cycle = 0;
            }
        }

        let high_gpr_pressure = ssa_live.is_some() && cur_gpr_live >= SSA_GPR_PRESSURE_THRESHOLD;
        let high_fp_pressure = ssa_live.is_some() && cur_fp_live >= SSA_FP_PRESSURE_THRESHOLD;
        let high_pressure = high_gpr_pressure || high_fp_pressure;

        let mut best_u = usize::MAX;
        if high_pressure {
            let critical_gpr_pressure =
                ssa_live.is_some() && cur_gpr_live >= SSA_GPR_CRITICAL_THRESHOLD;
            let critical_fp_pressure =
                ssa_live.is_some() && cur_fp_live >= SSA_FP_CRITICAL_THRESHOLD;
            let critical_pressure = critical_gpr_pressure || critical_fp_pressure;
            // Pre-RA SSA two-level register-pressure mode:
            //   Critical (`cur_gpr_live >= 21` or `cur_fp_live >= 22`):
            //     Tier 3: pressure-reducing (`active_delta < 0`, kills live VRegs; ties by `rec_height`)
            //     Tier 2: pressure-neutral (`active_delta == 0`, recurrences first via `rec_height`)
            //     Tier 1: pressure-increasing recurrence (`rec_height > 0 && active_delta > 0`)
            //     Tier 0: pressure-increasing non-critical op (`active_delta > 0`)
            //   Moderate (`18..21` live GPRs or `20..22` live FPs):
            //     Tier 3: critical loop-carried recurrence (`rec_height > 0`) — never delayed!
            //     Tier 2: pressure-reducing (`active_delta < 0`, kills live VRegs!)
            //     Tier 1: pressure-neutral (`active_delta == 0`)
            //     Tier 0: pressure-increasing non-critical load/ALU (`active_delta > 0`)
            let mut best_key: (bool, u8, i16, u16, bool, bool, u16, u8, bool, u16, usize) =
                (false, 0, i16::MIN, 0, false, false, 0, 0, false, 0, 0);
            let mut cand_mask = ready;
            while cand_mask != 0 {
                let u = to_idx(cand_mask.trailing_zeros());
                cand_mask &= cand_mask - 1;
                let u_info = at(&info, u, InstInfo::EMPTY);
                let not_deferred_ptr_step = ((at(&ptr_step_mem_peers, u, 0u128) & ready)
                    | (at(&ptr_step_store_peers, u, 0u128) & unscheduled))
                    == 0;

                let mut gpr_kills: i16 = 0;
                let mut fp_kills: i16 = 0;
                let mut gpr_defs: i16 = 0;
                let mut fp_defs: i16 = 0;
                if let Some(ssa) = ssa_live.as_deref() {
                    for &v in u_info.vreg_uses_slice() {
                        let vi = to_idx(v);
                        if vi < ssa.num_vregs
                            && at(&ssa.rem_uses, vi, 0u16) == 1
                            && let Some(&cls) = func.vreg_class.get(vi)
                        {
                            if cls.is_gpr() {
                                gpr_kills = gpr_kills.saturating_add(1);
                            } else if cls.is_fp_or_vec() {
                                fp_kills = fp_kills.saturating_add(1);
                            }
                        }
                    }
                    for &v in u_info.vreg_defs_slice() {
                        let vi = to_idx(v);
                        if vi < ssa.num_vregs
                            && let Some(&cls) = func.vreg_class.get(vi)
                        {
                            if cls.is_gpr() {
                                gpr_defs = gpr_defs.saturating_add(1);
                            } else if cls.is_fp_or_vec() {
                                fp_defs = fp_defs.saturating_add(1);
                            }
                        }
                    }
                }
                let gpr_delta = gpr_defs.saturating_sub(gpr_kills);
                let fp_delta = fp_defs.saturating_sub(fp_kills);
                let active_delta = match (high_gpr_pressure, high_fp_pressure) {
                    (true, true) => gpr_delta.saturating_add(fp_delta),
                    (true, false) => gpr_delta,
                    (false, true) => fp_delta,
                    (false, false) => 0,
                };

                let u_rec_h = at(&rec_height, u, 0u16);
                let rec_tie = i16::try_from(u_rec_h).unwrap_or(i16::MAX);
                let (pressure_tier, tier_tie) = if critical_pressure {
                    if active_delta < 0 {
                        (3u8, active_delta.saturating_neg())
                    } else if active_delta == 0 {
                        (2u8, rec_tie)
                    } else if u_rec_h > 0 {
                        (1u8, rec_tie)
                    } else {
                        (0u8, active_delta.saturating_neg())
                    }
                } else if u_rec_h > 0 {
                    (3u8, rec_tie)
                } else if active_delta < 0 {
                    (2u8, active_delta.saturating_neg())
                } else if active_delta == 0 {
                    (1u8, 0i16)
                } else {
                    (0u8, active_delta.saturating_neg())
                };

                let u_ra = at(&ready_at, u, 0u16);
                let is_ready_now = u_ra <= cycle;
                let p_idx = u_info.pipe.index();
                let used = at(&pipe_used, p_idx, 0u8);
                let cap = at(&pipe_caps, p_idx, 1u8);
                let pipe_free = used < cap;
                let pipe_avail = cap.saturating_sub(used);
                let load_first = matches!(u_info.mem, MemKind::Load);
                let earliest_ready = u16::MAX.saturating_sub(u_ra);
                let orig_tie = usize::MAX - u;

                let key = (
                    not_deferred_ptr_step,
                    pressure_tier,
                    tier_tie,
                    u_rec_h,
                    is_ready_now,
                    pipe_free,
                    at(&height, u, 0u16),
                    pipe_avail,
                    load_first,
                    earliest_ready,
                    orig_tie,
                );
                if best_u == usize::MAX || key > best_key {
                    best_u = u;
                    best_key = key;
                }
            }
        } else {
            // Aggressive latency + recurrence + port-balance mode (post-RA or SSA below pressure threshold).
            // `adrp_fuse` ranks highest in post-RA so `adrp + addlo12/ldr` pairs stay strictly glued;
            // `not_deferred_ptr_step` defers pre-RA `AddImm`/`SubImm` behind ready base-reading memory ops.
            let mut best_key: (bool, bool, bool, bool, u16, u8, bool, u16, usize) =
                (false, false, false, false, 0, 0, false, 0, 0);
            let mut cand_mask = ready;
            while cand_mask != 0 {
                let u = to_idx(cand_mask.trailing_zeros());
                cand_mask &= cand_mask - 1;
                let u_info = at(&info, u, InstInfo::EMPTY);

                let adrp_fuse = fuses_adrp_with_prev(&ctx, prev_op_idx, u);
                let not_deferred_ptr_step = ((at(&ptr_step_mem_peers, u, 0u128) & ready)
                    | (at(&ptr_step_store_peers, u, 0u128) & unscheduled))
                    == 0;
                let eff_ra = effective_ready_at(&ctx, prev_op_idx, u, at(&ready_at, u, 0u16), cycle);
                let is_ready_now = eff_ra <= cycle;
                let p_idx = u_info.pipe.index();
                let used = at(&pipe_used, p_idx, 0u8);
                let cap = at(&pipe_caps, p_idx, 1u8);
                let pipe_free = used < cap;
                let pipe_avail = cap.saturating_sub(used);
                let load_first = matches!(u_info.mem, MemKind::Load);
                let earliest_ready = u16::MAX.saturating_sub(eff_ra);
                let orig_tie = usize::MAX - u;
                let key = (
                    adrp_fuse,
                    not_deferred_ptr_step,
                    is_ready_now,
                    pipe_free,
                    at(&height, u, 0u16),
                    pipe_avail,
                    load_first,
                    earliest_ready,
                    orig_tie,
                );
                if best_u == usize::MAX || key > best_key {
                    best_u = u;
                    best_key = key;
                }
            }
        }

        set(&mut order, step, best_u);
        ready &= !(1u128 << best_u);
        unscheduled &= !(1u128 << best_u);

        let best_info = at(&info, best_u, InstInfo::EMPTY);
        let fused_adrp = fuses_adrp_with_prev(&ctx, prev_op_idx, best_u);
        let eff_ra = effective_ready_at(&ctx, prev_op_idx, best_u, at(&ready_at, best_u, 0u16), cycle);
        let p_idx = best_info.pipe.index();
        let extra_decode: u8 = if fused_adrp { 0 } else { 1 };
        let extra_pipe: u8 =
            if fused_adrp && matches!(ctx.ops.get(best_u), Some(MOp::AddLo12 { .. })) { 0 } else { 1 };

        if eff_ra > cycle {
            cycle = eff_ra;
            pipe_used = [0; 4];
            issued_in_cycle = 0;
        } else if issued_in_cycle.saturating_add(extra_decode) > issue_width
            || at(&pipe_used, p_idx, 0u8).saturating_add(extra_pipe) > at(&pipe_caps, p_idx, 1u8)
        {
            cycle = cycle.saturating_add(1);
            pipe_used = [0; 4];
            issued_in_cycle = 0;
        }
        add_at_u8(&mut pipe_used, p_idx, extra_pipe);
        issued_in_cycle = issued_in_cycle.saturating_add(extra_decode);
        prev_op_idx = u8_of(best_u);

        // Update SSA live GPR/FP VReg counts after scheduling `best_u`.
        if let Some(ssa) = ssa_live.as_deref_mut() {
            for &v in best_info.vreg_uses_slice() {
                let vi = to_idx(v);
                if vi < ssa.num_vregs {
                    let new_rem = if let Some(slot) = ssa.rem_uses.get_mut(vi) {
                        *slot = slot.saturating_sub(1);
                        *slot
                    } else {
                        u16::MAX
                    };
                    if new_rem == 0 && at(&ssa.vreg_live, vi, false) {
                        set(&mut ssa.vreg_live, vi, false);
                        if let Some(&cls) = func.vreg_class.get(vi) {
                            if cls.is_gpr() {
                                cur_gpr_live = cur_gpr_live.saturating_sub(1);
                            } else if cls.is_fp_or_vec() {
                                cur_fp_live = cur_fp_live.saturating_sub(1);
                            }
                        }
                    }
                }
            }
            for &v in best_info.vreg_defs_slice() {
                let vi = to_idx(v);
                if vi < ssa.num_vregs
                    && at(&ssa.rem_uses, vi, 0u16) > 0
                    && !at(&ssa.vreg_live, vi, true)
                {
                    set(&mut ssa.vreg_live, vi, true);
                    if let Some(&cls) = func.vreg_class.get(vi) {
                        if cls.is_gpr() {
                            cur_gpr_live = cur_gpr_live.saturating_add(1);
                        } else if cls.is_fp_or_vec() {
                            cur_fp_live = cur_fp_live.saturating_add(1);
                        }
                    }
                }
            }
        }

        let u_lat = u16::from(best_info.lat);
        let mut smask = at(&succs, best_u, 0u128);
        let raw_mask = at(&is_raw, best_u, 0u128);
        while smask != 0 {
            let b = to_idx(smask.trailing_zeros());
            smask &= smask - 1;
            let edge_w = if ((raw_mask >> b) & 1) != 0 { u_lat } else { 0 };
            let avail = cycle.saturating_add(edge_w);
            max_at_u16(&mut ready_at, b, avail);
            if let Some(slot) = pred_rem.get_mut(b) {
                *slot = slot.saturating_sub(1);
                if *slot == 0 {
                    ready |= 1u128 << b;
                }
            }
        }
    }

    if let Some(ssa) = ssa_live.as_deref_mut() {
        ssa.cleanup_window(&info, len);
    }

    // 4b. Post-RA Bounded Optimal Beam-4 Lookahead on hot inner loops (`4..=96` insts):
    //     compare exact simulated `(makespan, rec_done, last_issue_cycle, neg_fused)` against
    //     greedy and emit beam only when strictly better (guaranteed never worse than greedy!).
    //     Strictly guarded to `!ctx.is_ssa` so beam-4 never increases pre-RA register pressure.
    if !ctx.is_ssa
        && (in_loop || has_any_recurrence)
        && (BEAM_MIN_LEN..=BEAM_MAX_LEN).contains(&len)
    {
        let greedy_cost = simulate_schedule_cost(&ctx, order.get(..len).unwrap_or(&[]));
        if let Some((beam_order, beam_cost)) = beam_search_window(&ctx, &init_pred_rem)
            && beam_cost < greedy_cost
        {
            order = beam_order;
        }
    }

    stats.regions_scheduled = stats.regions_scheduled.saturating_add(1);

    // 5. Commit lockstep SoA permutation only when at least one instruction moved.
    let mut moved = 0u32;
    for (step, &orig) in order.iter().enumerate().take(len) {
        if orig != step {
            moved = moved.saturating_add(1);
        }
    }
    if moved == 0 {
        return;
    }

    let mut saved_ops = [MOp::Nop; WINDOW_MAX];
    let mut saved_flags = [MInstFlags::NONE; WINDOW_MAX];
    let mut saved_vflag_def = [NO_VFLAG; WINDOW_MAX];
    let mut saved_vflag_use = [NO_VFLAG; WINDOW_MAX];
    let mut saved_note_idx = [NO_NOTE; WINDOW_MAX];

    for i in 0..len {
        let g = w_start + i;
        set(&mut saved_ops, i, at(&func.ops, g, MOp::Nop));
        set(&mut saved_flags, i, at(&func.flags, g, MInstFlags::NONE));
        set(&mut saved_vflag_def, i, at(&func.vflag_def, g, NO_VFLAG));
        set(&mut saved_vflag_use, i, at(&func.vflag_use, g, NO_VFLAG));
        set(&mut saved_note_idx, i, at(&func.note_idx, g, NO_NOTE));
    }

    for step in 0..len {
        let orig = at(&order, step, step);
        let dst = w_start + step;
        set(&mut func.ops, dst, at(&saved_ops, orig, MOp::Nop));
        set(&mut func.flags, dst, at(&saved_flags, orig, MInstFlags::NONE));
        set(&mut func.vflag_def, dst, at(&saved_vflag_def, orig, NO_VFLAG));
        set(&mut func.vflag_use, dst, at(&saved_vflag_use, orig, NO_VFLAG));
        set(&mut func.note_idx, dst, at(&saved_note_idx, orig, NO_NOTE));
    }

    stats.insts_reordered = stats.insts_reordered.saturating_add(moved);
}

// -----------------------------------------------------------------------------
// 5. Public Entry Point (`schedule_mfunc`)
// -----------------------------------------------------------------------------

/// Runs data-driven target scheduling over every basic block of `func`, combining pre-RA SSA
/// register-pressure control, post-RA bounded Beam-4 lookahead on hot inner loops, loop-carried
/// recurrence hoisting, single-live-`VFlag` NZCV serialization, and Apple Silicon / Graviton 5
/// macro-op fusion while reordering all SoA instruction columns (`ops`, `flags`, `vflag_def`,
/// `vflag_use`, `note_idx`) in lockstep in strictly `O(N)` time.
pub fn schedule_mfunc<'a>(
    func: &mut MFunc<'a>,
    target: TargetModel,
    alloc: Alloc<'a>,
) -> MSchedStats {
    let mut stats = MSchedStats::default();
    let n_ops = func.ops.len();
    let n_blocks = func.block_insts.len();

    if n_ops == 0
        || n_blocks == 0
        || func.block_order.is_empty()
        || func.flags.len() != n_ops
        || func.vflag_def.len() != n_ops
        || func.vflag_use.len() != n_ops
        || func.note_idx.len() != n_ops
    {
        return stats;
    }

    // 1. Exact loop-carried recurrence & natural-loop analysis across all backedges `b -> h`.
    let loop_analysis = analyze_loop_recurrences(func, target, alloc);
    for &boosted in loop_analysis.is_recurrence.iter() {
        if boosted {
            stats.recurrences_boosted = stats.recurrences_boosted.saturating_add(1);
        }
    }

    // 1b. Precompute bounded SSA backward liveness when scheduling pre-RA (`func.is_ssa == true`).
    let mut ssa_live = SsaLiveness::new(func, target, alloc);

    // 2. Schedule every unique block in `block_order`, splitting at barriers and into <= 128-inst
    //    windows (walked from last window down to first so backward SSA liveness stays exact).
    let mut visited_block = vec_filled(n_blocks, false, alloc);
    let num_ord = func.block_order.len();
    for ord_idx in 0..num_ord {
        let Some(&blk) = func.block_order.get(ord_idx) else {
            continue;
        };
        let b = to_idx(blk.0);
        if b >= n_blocks || at(&visited_block, b, true) {
            continue;
        }
        set(&mut visited_block, b, true);

        let range = func.block_inst_range(blk);
        let b_start = range.start.min(n_ops);
        let b_end = range.end.min(n_ops);
        if b_end <= b_start + 1 {
            continue;
        }

        if let Some(ssa) = ssa_live.as_mut() {
            ssa.init_block(b);
        }
        let in_loop = loop_analysis.in_loop_block.get(b).copied().unwrap_or(false);

        let mut pos = b_end;
        while pos > b_start {
            let idx = pos - 1;
            if is_sched_barrier(func, idx) {
                if let Some(ssa) = ssa_live.as_mut() {
                    ssa.step_backward_inst(func, idx, target);
                }
                pos = idx;
                continue;
            }
            let r_end = pos;
            let mut r_start = idx;
            while r_start > b_start && !is_sched_barrier(func, r_start - 1) {
                r_start -= 1;
            }
            let r_len = r_end - r_start;
            if r_len <= 1 {
                if let Some(ssa) = ssa_live.as_mut() {
                    ssa.step_backward_inst(func, r_start, target);
                }
                pos = r_start;
                continue;
            }

            let followed_by_flag_branch =
                r_end < b_end && func.ops.get(r_end).is_some_and(is_flag_cond_branch);
            let followed_by_cbz_rt =
                if r_end < b_end { func.ops.get(r_end).and_then(cbz_tested_reg) } else { None };

            // Sub-partition giant barrier-free regions into forward-aligned `<= WINDOW_MAX` windows.
            let num_windows = r_len.div_ceil(WINDOW_MAX);
            for w_idx in (0..num_windows).rev() {
                let w_start = r_start + w_idx * WINDOW_MAX;
                let w_end = w_start.saturating_add(WINDOW_MAX).min(r_end);
                if w_end <= w_start + 1 {
                    if let Some(ssa) = ssa_live.as_mut() {
                        for k in (w_start..w_end).rev() {
                            ssa.step_backward_inst(func, k, target);
                        }
                    }
                    continue;
                }
                let is_last_window = w_idx + 1 == num_windows;
                schedule_window(
                    func,
                    w_start,
                    w_end,
                    is_last_window && followed_by_flag_branch,
                    if is_last_window { followed_by_cbz_rt } else { None },
                    in_loop,
                    &loop_analysis.is_recurrence,
                    ssa_live.as_mut(),
                    target,
                    &mut stats,
                );
            }
            pos = r_start;
        }
    }

    stats
}

// -----------------------------------------------------------------------------
// 6. Unit & Hardware Tests (NO `Global` / NO `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    use blaze_tier::{Arch, Machine, Os, RelocKind};

    use super::{
        CalibratedTargetOpts, InstInfo, MemKind, TargetModel, WINDOW_MAX, WindowCtx,
        cbz_tested_reg, classify_inst, is_flag_cond_branch, schedule_mfunc,
        simulate_schedule_cost, to_idx, u32_of, vreg_slices_intersect,
    };
    use crate::sota::mfunc::{
        AddrMode, DEFAULT_RET_LIVE_GPRS, LogicalKind, MCond, MFunc, MFuncOpts, MInstFlags,
        MNoteKind, MOp, MReg, MShift, MemSize, NO_NOTE, PReg, ShiftKind, VReg, VRegClass,
        mencode, mverify,
    };

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn new_post_ra(os: Os) -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os,
            is_ssa: false,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    fn new_ssa(os: Os) -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os,
            is_ssa: true,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    #[test]
    fn target_model_from_machine_covers_apple_m5_and_graviton5() {
        let mac = TargetModel::from_machine(Machine {
            arch: Arch::Arm64,
            os: Os::MacOs,
            features: Machine::LSE | Machine::CSSC,
        });
        assert_eq!(mac, TargetModel::APPLE_M5);
        assert_eq!(mac.decode_width, 10);
        assert_eq!(mac.page_size, 16384);
        assert!(!mac.has_sve2);

        let grav = TargetModel::from_machine(Machine {
            arch: Arch::Arm64,
            os: Os::Linux,
            features: Machine::LSE | Machine::CSSC,
        });
        assert_eq!(grav, TargetModel::GRAVITON5_NEOVERSE_V3);
        assert_eq!(grav.decode_width, 8);
        assert_eq!(grav.page_size, 4096);
        assert!(grav.has_sve2);

        let baseline = TargetModel::from_machine(Machine {
            arch: Arch::Arm64,
            os: Os::MacOs,
            features: Machine::LSE,
        });
        assert!(baseline.has_lse);
        assert!(!baseline.has_cssc);
    }

    #[test]
    fn msched_hoists_loop_carried_recurrence_ahead_of_unrelated_alu() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Entry: x0 = n, x1 = state; init x2 = bits_consumed (0), x3 = 3, x13 = 0.
        func.begin_block(b_entry);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X2), imm: 0, wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X3), imm: 3, wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X13), imm: 0, wide: true }, MInstFlags::NONE);
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        // Loop body:
        //   [0]  add x4, x1, x2            (reads loop-carried `state` x1 and `bits_consumed` x2)
        //   [1..=8] 8 unrelated non-loop-carried ALU ops forming a height-8 chain (`x3 -> x6..x13`)
        //   [9]  add x2, x2, #7            (loop-carried `bits_consumed` update -> MUST hoist!)
        //   [10] eor x1, x4, x3            (loop-carried `state` update -> MUST hoist!)
        //   [11] subs x0, x0, #1           (macro-op fused with b.ne -> stays at end)
        //   [12] b.ne b_loop
        let bits_update_op = MOp::AddImm {
            dst: MReg::p(PReg::X2),
            rn: MReg::p(PReg::X2),
            imm12: 7,
            shift12: false,
            wide: true,
            sets_flags: false,
        };
        let state_update_op = MOp::LogicalReg {
            dst: MReg::p(PReg::X1),
            rn: MReg::p(PReg::X4),
            rm: MReg::p(PReg::X3),
            shift: None,
            invert_rm: false,
            kind: LogicalKind::Eor,
            wide: true,
        };
        let subs_op = MOp::SubImm {
            dst: MReg::p(PReg::X0),
            rn: MReg::p(PReg::X0),
            imm12: 1,
            shift12: false,
            wide: true,
            sets_flags: true,
        };

        func.begin_block(b_loop);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X4),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // 8 unrelated non-loop-carried ALU ops forming a long chain of height 8
        // (would bury height-1 loop updates at the bottom without the +12 recurrence boost!).
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X6), rn: MReg::p(PReg::X3), imm12: 1, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X7), rn: MReg::p(PReg::X6), imm12: 2, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X8), rn: MReg::p(PReg::X7), imm12: 3, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X8), imm12: 4, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X10), rn: MReg::p(PReg::X9), imm12: 5, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X11), rn: MReg::p(PReg::X10), imm12: 6, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X12), rn: MReg::p(PReg::X11), imm12: 7, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::p(PReg::X13), rn: MReg::p(PReg::X12), imm12: 8, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        // Loop-carried recurrences originally placed at the very bottom!
        let bits_inst = func.push_inst(bits_update_op, MInstFlags::NONE);
        func.attach_note(
            bits_inst,
            MNoteKind::Reloc { kind: RelocKind::Page21, target: 42, addend: 0 },
        );
        func.push_inst(state_update_op, MInstFlags::NONE);
        let i_subs = func.push_inst(subs_op, MInstFlags::NONE);
        func.set_vflag_def(i_subs, 0);
        let i_br = func.push_inst(MOp::BCond { cond: MCond::Ne, target: b_loop }, MInstFlags::TERMINATOR);
        func.set_vflag_use(i_br, 0);
        func.end_block(b_loop);

        // Exit: `add x0, x1, x2; add x0, x0, x13; ret`
        func.begin_block(b_exit);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X13),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.recurrences_boosted >= 2, "expected loop-carried recurrences boosted: {stats:?}");
        assert!(stats.insts_reordered > 0, "expected instructions reordered: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));

        // Verify both recurrence updates hoisted early ahead of the 8-instruction unrelated ALU chain!
        let loop_range = func.block_inst_range(b_loop);
        let loop_ops = &func.ops[loop_range.start..loop_range.end];
        let mut bits_pos = usize::MAX;
        let mut state_pos = usize::MAX;
        for (rel, &op) in loop_ops.iter().enumerate() {
            if op == bits_update_op {
                bits_pos = rel;
            }
            if op == state_update_op {
                state_pos = rel;
            }
        }
        assert!(bits_pos <= 2, "bits_consumed recurrence must hoist early, landed at {bits_pos}");
        assert!(state_pos <= 4, "state recurrence must hoist early, landed at {state_pos}");
        // Verify attached `MNote` moved in lockstep with `bits_update_op`!
        assert_ne!(func.note_idx[loop_range.start + bits_pos], NO_NOTE);
        // Verify `subs` remained locked immediately before `b.ne` for macro-op fusion!
        assert_eq!(loop_ops[loop_ops.len() - 2], subs_op);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let enc_out = mencode(&func, alloc).expect("mencode must succeed after scheduling");
            let mut bytes: blaze_own::Vec<u8, Alloc<'static>> =
                blaze_own::Vec::with_capacity_in(enc_out.words.len() * 4, alloc);
            for &w in enc_out.words.iter() {
                for b in w.to_le_bytes() {
                    bytes.push(b);
                }
            }
            let exec = blaze_inproc::exec::Executable::new(&bytes, 0).expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 frameless leaf touching only caller-saved `X0..=X13`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };

            for &n in &[0u64, 1, 2, 5, 17, 100] {
                let init_state = 0x1234_u64;
                let mut state = init_state;
                let mut bits = 0u64;
                let mut x13 = 0u64;
                for _ in 0..n {
                    let x4 = state.wrapping_add(bits);
                    x13 = 3u64 + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8;
                    bits = bits.wrapping_add(7);
                    state = x4 ^ 3;
                }
                let expected = state.wrapping_add(bits).wrapping_add(x13);
                assert_eq!(f(n, init_state), expected, "hardware mismatch for n={n}");
            }
        }
    }

    #[test]
    fn msched_preserves_macro_op_fusion_and_single_live_vflag() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Two flag-setting chains in one region:
        //   Chain 0: `subs x1, x1, #1` (vflag 0) -> `csel x2, x3, x4, eq` (vflag 0)
        //   Chain 1: `cmp x1, x0`      (vflag 1) -> trailing unrelated ops -> `b.lt b_loop` (vflag 1)
        // Plus independent multi-cycle `mul` ops that want to hoist.
        // Scheduler must NEVER interleave vflag 0 and vflag 1, AND must move `cmp` right before `b.lt`!
        let subs_op = MOp::SubImm {
            dst: MReg::p(PReg::X1),
            rn: MReg::p(PReg::X1),
            imm12: 1,
            shift12: false,
            wide: true,
            sets_flags: true,
        };
        let csel_op = MOp::Csel {
            dst: MReg::p(PReg::X2),
            rn: MReg::p(PReg::X3),
            rm: MReg::p(PReg::X4),
            cond: MCond::Eq,
            wide: true,
        };
        let cmp_op = MOp::CmpReg {
            rn: MReg::p(PReg::X1),
            rm: MReg::p(PReg::X0),
            shift: None,
            wide: true,
        };

        func.begin_block(b_loop);
        let i_subs = func.push_inst(subs_op, MInstFlags::NONE);
        func.set_vflag_def(i_subs, 0);
        let i_csel = func.push_inst(csel_op, MInstFlags::NONE);
        func.set_vflag_use(i_csel, 0);
        let i_cmp = func.push_inst(cmp_op, MInstFlags::NONE);
        func.set_vflag_def(i_cmp, 1);
        // Trailing independent work placed AFTER `cmp` so `cmp` is NOT initially adjacent to `b.lt`.
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X5), rn: MReg::p(PReg::X6), rm: MReg::p(PReg::X7), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X8),
                rn: MReg::p(PReg::X5),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X8),
                imm12: 5,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let i_br = func.push_inst(MOp::BCond { cond: MCond::Lt, target: b_loop }, MInstFlags::TERMINATOR);
        func.set_vflag_use(i_br, 1);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X2), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.insts_reordered > 0, "expected cmp to move to fuse with b.lt");
        assert_eq!(mverify(&func), Ok(()), "single-live-VFlag invariant must hold after scheduling");

        let range = func.block_inst_range(b_loop);
        let ops = &func.ops[range.start..range.end];
        let mut pos_subs = usize::MAX;
        let mut pos_csel = usize::MAX;
        let mut pos_cmp = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == subs_op {
                pos_subs = rel;
            } else if op == csel_op {
                pos_csel = rel;
            } else if op == cmp_op {
                pos_cmp = rel;
            }
        }
        assert!(pos_subs < pos_csel, "subs must precede csel");
        assert!(pos_csel < pos_cmp, "csel (vflag 0) must precede cmp (vflag 1): no interleaving!");
        assert_eq!(
            pos_cmp,
            ops.len() - 2,
            "cmp must be locked immediately before b.lt for macro-op fusion"
        );
    }

    #[test]
    fn msched_handles_1000_inst_block_linearly() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::Linux);
        let b0 = func.new_block(false);

        func.begin_block(b0);
        // Emit 1,000 non-barrier instructions including cross-window VFlag chains, loads, muls, and ALU ops.
        let mut next_vflag = 0u32;
        for i in 0..1000usize {
            let rem = i % 64;
            if rem == 62 {
                // Place `cmp` near the end of window `k` and its `csel` at index 63 (or across boundary!).
                let cmp = func.push_inst(
                    MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 7, shift12: false, wide: true },
                    MInstFlags::NONE,
                );
                func.set_vflag_def(cmp, next_vflag);
            } else if rem == 63 {
                // Keep `vflag` live across the 64-inst window boundary when `i % 128 == 63`!
                if (i % 128) == 63 {
                    func.push_inst(
                        MOp::AddImm {
                            dst: MReg::p(PReg::X4),
                            rn: MReg::p(PReg::X4),
                            imm12: 1,
                            shift12: false,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    let sel = func.push_inst(
                        MOp::Csel {
                            dst: MReg::p(PReg::X1),
                            rn: MReg::p(PReg::X0),
                            rm: MReg::p(PReg::X2),
                            cond: MCond::Eq,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    func.set_vflag_use(sel, next_vflag);
                    next_vflag += 1;
                }
            } else if rem == 0 && i > 0 && (i % 128) == 64 {
                // Consume the `vflag` defined at index 62 in the previous window!
                let sel = func.push_inst(
                    MOp::Csel {
                        dst: MReg::p(PReg::X1),
                        rn: MReg::p(PReg::X0),
                        rm: MReg::p(PReg::X2),
                        cond: MCond::Ne,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                func.set_vflag_use(sel, next_vflag);
                next_vflag += 1;
            } else if (i % 5) == 0 {
                func.push_inst(
                    MOp::Mul {
                        dst: MReg::p(PReg::X5),
                        rn: MReg::p(PReg::X2),
                        rm: MReg::p(PReg::X3),
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
            } else if (i % 5) == 1 {
                func.push_inst(
                    MOp::AddReg {
                        dst: MReg::p(PReg::X6),
                        rn: MReg::p(PReg::X5),
                        rm: MReg::p(PReg::X4),
                        shift: None,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
            } else if (i % 5) == 2 {
                func.push_inst(
                    MOp::Ldr {
                        dst: MReg::p(PReg::X7),
                        addr: AddrMode::BaseOff { base: MReg::SP, off: 0 },
                        size: MemSize::X64,
                        signed: false,
                    },
                    MInstFlags::MEM_READ,
                );
            } else if (i % 5) == 3 {
                func.push_inst(
                    MOp::AddReg {
                        dst: MReg::p(PReg::X8),
                        rn: MReg::p(PReg::X7),
                        rm: MReg::p(PReg::X6),
                        shift: None,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
            } else {
                func.push_inst(
                    MOp::AddImm {
                        dst: MReg::p(PReg::X9),
                        rn: MReg::p(PReg::X9),
                        imm12: 3,
                        shift12: false,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
            }
        }
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::GRAVITON5_NEOVERSE_V3, alloc);
        let expected_windows = u32_of(1000usize.div_ceil(WINDOW_MAX));
        assert!(
            stats.regions_scheduled >= expected_windows,
            "expected >= {expected_windows} sliding windows for 1000 insts: {stats:?}"
        );
        assert!(stats.insts_reordered > 0, "expected reordering across 1000-inst block: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));
        assert!(mencode(&func, alloc).is_ok(), "1000-inst scheduled block must encode cleanly");
    }

    #[test]
    fn msched_boosts_ssa_parallel_copy_loop_carried_vreg() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let v_state = func.alloc_vreg(VRegClass::Gpr64);
        let v_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_tmp = func.alloc_vreg(VRegClass::Gpr64);
        let v_u0 = func.alloc_vreg(VRegClass::Gpr64);
        let v_u1 = func.alloc_vreg(VRegClass::Gpr64);
        let v_u2 = func.alloc_vreg(VRegClass::Gpr64);
        let v_u3 = func.alloc_vreg(VRegClass::Gpr64);
        let v_u4 = func.alloc_vreg(VRegClass::Gpr64);
        let v_cnt = func.alloc_vreg(VRegClass::Gpr64);
        let v_cnt_next = func.alloc_vreg(VRegClass::Gpr64);

        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        let next_state_op = MOp::AddImm {
            dst: MReg::V(v_next),
            rn: MReg::V(v_tmp),
            imm12: 11,
            shift12: false,
            wide: true,
            sets_flags: false,
        };

        func.begin_block(b_loop);
        func.push_inst(
            MOp::AddImm { dst: MReg::V(v_tmp), rn: MReg::V(v_state), imm12: 1, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        // Unrelated height-5 chain on `v_u0..v_u4`.
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_u0), imm: 10, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::V(v_u1), rn: MReg::V(v_u0), imm12: 1, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::V(v_u2), rn: MReg::V(v_u1), imm12: 2, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::V(v_u3), rn: MReg::V(v_u2), imm12: 3, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm { dst: MReg::V(v_u4), rn: MReg::V(v_u3), imm12: 4, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        // Recurrence-closing SSA VReg defs placed at bottom before latch `ParallelCopy`.
        func.push_inst(next_state_op, MInstFlags::NONE);
        func.push_inst(
            MOp::SubImm { dst: MReg::V(v_cnt_next), rn: MReg::V(v_cnt), imm12: 1, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_state), MReg::V(v_next)), (MReg::V(v_cnt), MReg::V(v_cnt_next))],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbnz { rt: MReg::V(v_cnt), target: b_loop, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.recurrences_boosted >= 2, "expected SSA ParallelCopy recurrences boosted: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));

        let range = func.block_inst_range(b_loop);
        let ops = &func.ops[range.start..range.end];
        let mut pos_next = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == next_state_op {
                pos_next = rel;
            }
        }
        assert!(pos_next <= 3, "SSA loop-carried VReg update must hoist early, landed at {pos_next}");
    }

    fn simulate_window_makespan(
        func: &MFunc<'_>,
        w_start: usize,
        w_end: usize,
        b_end: usize,
        target: TargetModel,
    ) -> u16 {
        let len = w_end - w_start;
        let mut info = [InstInfo::EMPTY; WINDOW_MAX];
        for a in 0..len {
            info[a] = classify_inst(func, w_start + a, target);
        }
        let mut succs = [0u128; WINDOW_MAX];
        let mut is_raw = [0u128; WINDOW_MAX];
        for a in 0..len {
            let ia = info[a];
            let a_vdefs = &ia.vreg_defs[..usize::from(ia.vreg_def_len)];
            let a_vuses = &ia.vreg_uses[..usize::from(ia.vreg_use_len)];
            for b in (a + 1)..len {
                let ib = info[b];
                let b_vdefs = &ib.vreg_defs[..usize::from(ib.vreg_def_len)];
                let b_vuses = &ib.vreg_uses[..usize::from(ib.vreg_use_len)];
                let raw = (ia.phys_gpr_defs & ib.phys_gpr_uses) != 0
                    || (ia.phys_fp_defs & ib.phys_fp_uses) != 0
                    || vreg_slices_intersect(a_vdefs, b_vuses)
                    || (ia.writes_flags && ib.reads_flags);
                let war = (ia.phys_gpr_uses & ib.phys_gpr_defs) != 0
                    || (ia.phys_fp_uses & ib.phys_fp_defs) != 0
                    || vreg_slices_intersect(a_vuses, b_vdefs)
                    || (ia.reads_flags && ib.writes_flags);
                let waw = (ia.phys_gpr_defs & ib.phys_gpr_defs) != 0
                    || (ia.phys_fp_defs & ib.phys_fp_defs) != 0
                    || vreg_slices_intersect(a_vdefs, b_vdefs)
                    || (ia.writes_flags && ib.writes_flags);
                let mem_dep = !matches!(
                    (ia.mem, ib.mem),
                    (MemKind::None, _) | (_, MemKind::None) | (MemKind::Load, MemKind::Load)
                );
                let trap_dep = (ia.may_trap && ib.may_trap)
                    || (ia.may_trap && matches!(ib.mem, MemKind::Store))
                    || (matches!(ia.mem, MemKind::Store) && ib.may_trap);
                if raw || war || waw || mem_dep || trap_dep {
                    let b_bit = 1u128 << b;
                    succs[a] |= b_bit;
                    if raw {
                        is_raw[a] |= b_bit;
                    }
                }
            }
        }
        let zeros = [0u16; WINDOW_MAX];
        let ctx = WindowCtx {
            ops: &func.ops[w_start..w_end],
            info: &info,
            succs: &succs,
            is_raw: &is_raw,
            height: &zeros,
            pure_height: &zeros,
            rec_height: &zeros,
            pure_rec_height: &zeros,
            len,
            is_ssa: func.is_ssa,
            followed_by_flag_branch: w_end < b_end && is_flag_cond_branch(&func.ops[w_end]),
            followed_by_cbz_rt: if w_end < b_end {
                cbz_tested_reg(&func.ops[w_end])
            } else {
                None
            },
            pipe_caps: [
                target.alu_pipes.max(1),
                target.ldst_pipes.max(1),
                target.fp_pipes.max(1),
                target.mul_pipes.max(1),
            ],
            issue_width: target.decode_width.max(1),
            target,
        };
        let mut identity = [0usize; WINDOW_MAX];
        for (i, slot) in identity.iter_mut().enumerate().take(len) {
            *slot = i;
        }
        simulate_schedule_cost(&ctx, &identity[..len]).makespan
    }

    #[test]
    fn msched_ssa_pressure_mode_prefers_vreg_killing_inst_above_threshold() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        // Allocate 22 live-in GPR VRegs + base pointer + loop-carried state & counter (> 20 GPR threshold).
        let mut v_live = [VReg(0); 22];
        for slot in v_live.iter_mut() {
            *slot = func.alloc_vreg(VRegClass::Gpr64);
        }
        let v_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_state = func.alloc_vreg(VRegClass::Gpr64);
        let v_state_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_cnt = func.alloc_vreg(VRegClass::Gpr64);
        let v_cnt_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_load_dst = func.alloc_vreg(VRegClass::Gpr64);
        let v_load_use = func.alloc_vreg(VRegClass::Gpr64);
        let v_kill_a = func.alloc_vreg(VRegClass::Gpr64);
        let v_kill_b = func.alloc_vreg(VRegClass::Gpr64);
        let v_kill_dst = func.alloc_vreg(VRegClass::Gpr64);

        // Two-block loop (`b_hdr -> b_body -> b_hdr`): `v_kill_a` and `v_kill_b` are produced in
        // `b_hdr` and consumed solely in `b_body`, so they are live-in to `b_body` but NOT live-out
        // on the back-edge to `b_hdr` (hence `kill_op` genuinely reduces live GPR pressure by 1).
        let b_hdr = func.new_block(true);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Non-critical new-def load (`pressure_delta = +1`, `height = 5`, `load_first = true`).
        // Without SSA pressure control, latency-first scheduling would always hoist this load first.
        let non_crit_load_op = MOp::Ldr {
            dst: MReg::V(v_load_dst),
            addr: AddrMode::BaseOff { base: MReg::V(v_base), off: 0 },
            size: MemSize::X64,
            signed: false,
        };
        let use_load_op = MOp::AddImm {
            dst: MReg::V(v_load_use),
            rn: MReg::V(v_load_dst),
            imm12: 3,
            shift12: false,
            wide: true,
            sets_flags: false,
        };
        // Pressure-reducing ALU: kills both `v_kill_a` and `v_kill_b`, defines `v_kill_dst` (`delta = -1`).
        let kill_op = MOp::AddReg {
            dst: MReg::V(v_kill_dst),
            rn: MReg::V(v_kill_a),
            rm: MReg::V(v_kill_b),
            shift: None,
            wide: true,
            sets_flags: false,
        };
        // Loop-carried recurrence updates (`rec_height > 0` — MUST still hoist!).
        let rec_state_op = MOp::AddImm {
            dst: MReg::V(v_state_next),
            rn: MReg::V(v_state),
            imm12: 7,
            shift12: false,
            wide: true,
            sets_flags: false,
        };
        let rec_cnt_op = MOp::SubImm {
            dst: MReg::V(v_cnt_next),
            rn: MReg::V(v_cnt),
            imm12: 1,
            shift12: false,
            wide: true,
            sets_flags: false,
        };

        func.begin_block(b_hdr);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_kill_a),
                rn: MReg::V(v_live[0]),
                imm12: 11,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_kill_b),
                rn: MReg::V(v_live[1]),
                imm12: 13,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        func.end_block(b_hdr);

        func.begin_block(b_loop);
        func.push_inst(non_crit_load_op, MInstFlags::MEM_READ);
        func.push_inst(use_load_op, MInstFlags::NONE);
        func.push_inst(kill_op, MInstFlags::NONE);
        func.push_inst(rec_state_op, MInstFlags::NONE);
        func.push_inst(rec_cnt_op, MInstFlags::NONE);
        func.push_parallel_copy(
            &[
                (MReg::V(v_state), MReg::V(v_state_next)),
                (MReg::V(v_cnt), MReg::V(v_cnt_next)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbnz { rt: MReg::V(v_cnt), target: b_hdr, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_loop);

        // Exit consumes `v_live[2..22]`, `v_base`, `v_kill_dst`, and `v_load_use` so all 25 GPR
        // VRegs (`v_live[0..22]`, `v_base`, `v_state`, `v_cnt`) are live entering `b_loop`.
        func.begin_block(b_exit);
        let mut acc = v_base;
        for &vl in &v_live[2..] {
            let next_acc = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next_acc),
                    rn: MReg::V(acc),
                    rm: MReg::V(vl),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next_acc;
        }
        let acc1 = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(acc1),
                rn: MReg::V(acc),
                rm: MReg::V(v_kill_dst),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let acc2 = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(acc2),
                rn: MReg::V(acc1),
                rm: MReg::V(v_load_use),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.recurrences_boosted >= 2, "expected loop recurrences boosted: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));

        let range = func.block_inst_range(b_loop);
        let ops = &func.ops[range.start..range.end];
        let mut pos_load = usize::MAX;
        let mut pos_kill = usize::MAX;
        let mut pos_rec = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == non_crit_load_op {
                pos_load = rel;
            } else if op == kill_op {
                pos_kill = rel;
            } else if op == rec_state_op {
                pos_rec = rel;
            }
        }
        assert!(
            pos_rec < pos_load,
            "loop-carried recurrence must still hoist ahead of non-critical load: rec={pos_rec}, load={pos_load}"
        );
        assert!(
            pos_kill < pos_load,
            "SSA pressure mode (>= 20 live GPRs) must pick VReg-killing op before non-critical load: kill={pos_kill}, load={pos_load}"
        );
    }

    #[test]
    fn msched_critical_ssa_pressure_kills_vregs_before_pressure_increasing_recurrence() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        // Allocate 20 live-in GPR VRegs + base pointer + counter + 2 loop-body-local inputs
        // (`cur_gpr_live = 24 >= SSA_GPR_CRITICAL_THRESHOLD = 21` at `b_loop` entry).
        let mut v_live = [VReg(0); 20];
        for slot in v_live.iter_mut() {
            *slot = func.alloc_vreg(VRegClass::Gpr64);
        }
        let v_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_state = func.alloc_vreg(VRegClass::Gpr64);
        let v_state_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_cnt = func.alloc_vreg(VRegClass::Gpr64);
        let v_cnt_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_load_dst = func.alloc_vreg(VRegClass::Gpr64);
        let v_kill_a = func.alloc_vreg(VRegClass::Gpr64);
        let v_kill_b = func.alloc_vreg(VRegClass::Gpr64);
        let v_kill_dst = func.alloc_vreg(VRegClass::Gpr64);

        let b_hdr = func.new_block(true);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Pressure-increasing loop-carried recurrence (`rec_height > 0`, `delta = +1`):
        // loads next `v_state_next` from loop-invariant `v_base` (which stays live-out).
        let rec_inc_op = MOp::Ldr {
            dst: MReg::V(v_state_next),
            addr: AddrMode::BaseOff { base: MReg::V(v_base), off: 0 },
            size: MemSize::X64,
            signed: false,
        };
        // Non-critical pressure-increasing load (`rec_height == 0`, `delta = +1`).
        let non_crit_load_op = MOp::Ldr {
            dst: MReg::V(v_load_dst),
            addr: AddrMode::BaseOff { base: MReg::V(v_base), off: 8 },
            size: MemSize::X64,
            signed: false,
        };
        // Pressure-neutral loop-carried recurrence (`rec_height > 0`, `delta = 0`):
        // kills `v_cnt` (overwritten by latch `ParallelCopy`) and defines `v_cnt_next`.
        let rec_neutral_op = MOp::SubImm {
            dst: MReg::V(v_cnt_next),
            rn: MReg::V(v_cnt),
            imm12: 1,
            shift12: false,
            wide: true,
            sets_flags: false,
        };
        // VReg-killing ALU (`rec_height == 0`, `delta = -1`): kills both `v_kill_a` and `v_kill_b`.
        let kill_op = MOp::AddReg {
            dst: MReg::V(v_kill_dst),
            rn: MReg::V(v_kill_a),
            rm: MReg::V(v_kill_b),
            shift: None,
            wide: true,
            sets_flags: false,
        };

        func.begin_block(b_hdr);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_kill_a),
                rn: MReg::V(v_state),
                imm12: 11,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_kill_b),
                rn: MReg::V(v_live[0]),
                imm12: 13,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        func.end_block(b_hdr);

        // Place `rec_inc_op` first and `kill_op` last so original order favors `rec_inc_op`.
        func.begin_block(b_loop);
        func.push_inst(rec_inc_op, MInstFlags::MEM_READ);
        func.push_inst(non_crit_load_op, MInstFlags::MEM_READ);
        func.push_inst(rec_neutral_op, MInstFlags::NONE);
        func.push_inst(kill_op, MInstFlags::NONE);
        func.push_parallel_copy(
            &[
                (MReg::V(v_state), MReg::V(v_state_next)),
                (MReg::V(v_cnt), MReg::V(v_cnt_next)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbnz { rt: MReg::V(v_cnt), target: b_hdr, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_loop);

        // Exit consumes `v_live[1..20]`, `v_base`, `v_kill_dst`, and `v_load_dst` so
        // `cur_gpr_live == 24 >= 21` entering `b_loop`.
        func.begin_block(b_exit);
        let mut acc = v_base;
        for &vl in &v_live[1..] {
            let next_acc = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next_acc),
                    rn: MReg::V(acc),
                    rm: MReg::V(vl),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next_acc;
        }
        let acc1 = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(acc1),
                rn: MReg::V(acc),
                rm: MReg::V(v_kill_dst),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let acc2 = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(acc2),
                rn: MReg::V(acc1),
                rm: MReg::V(v_load_dst),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.recurrences_boosted >= 2, "expected loop recurrences boosted: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));

        let range = func.block_inst_range(b_loop);
        let ops = &func.ops[range.start..range.end];
        let mut pos_kill = usize::MAX;
        let mut pos_rec_neutral = usize::MAX;
        let mut pos_rec_inc = usize::MAX;
        let mut pos_load = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == kill_op {
                pos_kill = rel;
            } else if op == rec_neutral_op {
                pos_rec_neutral = rel;
            } else if op == rec_inc_op {
                pos_rec_inc = rel;
            } else if op == non_crit_load_op {
                pos_load = rel;
            }
        }
        assert!(
            pos_kill < pos_rec_inc,
            "critical SSA pressure (>= 21 live GPRs) must schedule VReg-killing op before pressure-increasing recurrence: kill={pos_kill}, rec_inc={pos_rec_inc}"
        );
        assert!(
            pos_kill < pos_rec_neutral,
            "critical SSA pressure must schedule VReg-killing op (Tier 3) before pressure-neutral recurrence (Tier 2): kill={pos_kill}, rec_neutral={pos_rec_neutral}"
        );
        assert!(
            pos_rec_neutral < pos_rec_inc,
            "critical SSA pressure must schedule pressure-neutral recurrence (Tier 2) before pressure-increasing recurrence (Tier 1): rec_neutral={pos_rec_neutral}, rec_inc={pos_rec_inc}"
        );
        assert!(
            pos_rec_inc < pos_load,
            "critical SSA pressure must still schedule pressure-increasing recurrence (Tier 1) ahead of non-critical load (Tier 0): rec_inc={pos_rec_inc}, load={pos_load}"
        );
    }

    #[test]
    fn msched_beam_improves_or_matches_greedy_makespan_on_hot_loop() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Realistic hot inner loop mixing 4 loads (saturating 3 LDST pipes), 3 muls (saturating 2
        // MUL pipes), bitfield shifts (`ubfm`), loop-carried ptr & state recurrences (`x1`, `x2`),
        // attached `Trap` + `Reloc` notes, and macro-op fused `subs + b.ne`.
        let ldr0_op = MOp::Ldr {
            dst: MReg::p(PReg::X4),
            addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 0 },
            size: MemSize::X64,
            signed: false,
        };
        let acc1_op = MOp::AddReg {
            dst: MReg::p(PReg::X2),
            rn: MReg::p(PReg::X2),
            rm: MReg::p(PReg::X8),
            shift: None,
            wide: true,
            sets_flags: false,
        };
        let subs_op = MOp::SubImm {
            dst: MReg::p(PReg::X0),
            rn: MReg::p(PReg::X0),
            imm12: 1,
            shift12: false,
            wide: true,
            sets_flags: true,
        };

        func.begin_block(b_loop);
        // Short independent ALU + shift placed upfront in poor initial order.
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X3),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X11), rn: MReg::p(PReg::X10), immr: 3, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        let i_ldr0 = func.push_inst(ldr0_op, MInstFlags::MEM_READ);
        func.attach_note(i_ldr0, MNoteKind::Trap { trap_idx: 11 });
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X5),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X6),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 16 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X7),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 24 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X8), rn: MReg::p(PReg::X4), rm: MReg::p(PReg::X3), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X5), rm: MReg::p(PReg::X3), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X8), rn: MReg::p(PReg::X8), immr: 7, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X13), rn: MReg::p(PReg::X6), immr: 5, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X13), rn: MReg::p(PReg::X13), rm: MReg::p(PReg::X3), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X12),
                rn: MReg::p(PReg::X9),
                rm: MReg::p(PReg::X11),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X14),
                rn: MReg::p(PReg::X7),
                rm: MReg::p(PReg::X12),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X14),
                rn: MReg::p(PReg::X14),
                rm: MReg::p(PReg::X13),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let i_acc1 = func.push_inst(acc1_op, MInstFlags::NONE);
        func.attach_note(
            i_acc1,
            MNoteKind::Reloc { kind: RelocKind::Page21, target: 99, addend: 0 },
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X14),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                imm12: 32,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let i_subs = func.push_inst(subs_op, MInstFlags::NONE);
        func.set_vflag_def(i_subs, 0);
        let i_br = func.push_inst(MOp::BCond { cond: MCond::Ne, target: b_loop }, MInstFlags::TERMINATOR);
        func.set_vflag_use(i_br, 0);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X2), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let range_before = func.block_inst_range(b_loop);
        let unsched_makespan = simulate_window_makespan(
            &func,
            range_before.start,
            range_before.end - 1,
            range_before.end,
            TargetModel::APPLE_M5,
        );

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.recurrences_boosted >= 2, "expected loop recurrences boosted: {stats:?}");
        assert!(stats.insts_reordered > 0, "expected hot loop reordered: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));
        assert!(mencode(&func, alloc).is_ok(), "scheduled hot loop must encode cleanly");

        let range = func.block_inst_range(b_loop);
        let sched_makespan = simulate_window_makespan(
            &func,
            range.start,
            range.end - 1,
            range.end,
            TargetModel::APPLE_M5,
        );
        // Critical RAW chain is `ldr x5 (4) -> mul x9 (3) -> add x12 (1) -> add x14 (1) -> add x14 (1) -> add x2 (1)` = 11 cycles.
        assert_eq!(
            sched_makespan, 11,
            "expected exact optimal 11-cycle makespan (unscheduled was {unsched_makespan})"
        );
        assert!(sched_makespan <= unsched_makespan);

        let ops = &func.ops[range.start..range.end];
        let mut pos_ldr0 = usize::MAX;
        let mut pos_acc1 = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == ldr0_op {
                pos_ldr0 = rel;
            } else if op == acc1_op {
                pos_acc1 = rel;
            }
        }
        assert_ne!(pos_ldr0, usize::MAX);
        assert_ne!(pos_acc1, usize::MAX);
        // Verify both attached `MNote` records stayed attached to their exact instructions!
        let n_ldr0 = func.note_idx[range.start + pos_ldr0];
        let n_acc1 = func.note_idx[range.start + pos_acc1];
        assert_ne!(n_ldr0, NO_NOTE);
        assert_ne!(n_acc1, NO_NOTE);
        assert_eq!(func.notes[to_idx(n_ldr0)].kind, MNoteKind::Trap { trap_idx: 11 });
        assert_eq!(
            func.notes[to_idx(n_acc1)].kind,
            MNoteKind::Reloc { kind: RelocKind::Page21, target: 99, addend: 0 }
        );
        // Verify macro-op fused `subs` stayed glued immediately before `b.ne`.
        assert_eq!(ops[ops.len() - 2], subs_op);
    }

    #[test]
    fn msched_macro_op_fusion_and_calibrated_target_model() {
        let cal = TargetModel::calibrated(CalibratedTargetOpts {
            decode_width: 0,
            alu_pipes: 8,
            load_latency: 5,
            mul_latency: 4,
            ..CalibratedTargetOpts::from_base(TargetModel::APPLE_M5)
        });
        assert_eq!(cal.decode_width, 1, "zero decode_width must clamp to 1");
        assert_eq!(cal.alu_pipes, 8);
        assert_eq!(cal.load_latency, 5);
        assert_eq!(cal.mul_latency, 4);
        assert_eq!(cal.page_size, TargetModel::APPLE_M5.page_size);
        assert!(cal.macro_fuse_cmp_br);
        assert!(cal.macro_fuse_adrp_add);

        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b0 = func.new_block(false);
        let b1 = func.new_block(false);
        let b_exit = func.new_block(false);

        let adrp0 = MOp::Adrp { dst: MReg::p(PReg::X9) };
        let addlo0 = MOp::AddLo12 { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X9) };
        let adrp1 = MOp::Adrp { dst: MReg::p(PReg::X10) };
        let ldr_adrp1 = MOp::Ldr {
            dst: MReg::p(PReg::X11),
            addr: AddrMode::BaseOff { base: MReg::p(PReg::X10), off: 16 },
            size: MemSize::X64,
            signed: false,
        };
        let and_cbz = MOp::LogicalReg {
            dst: MReg::p(PReg::X1),
            rn: MReg::p(PReg::X2),
            rm: MReg::p(PReg::X3),
            shift: None,
            invert_rm: false,
            kind: LogicalKind::And,
            wide: true,
        };
        let ubfm_cbnz = MOp::Ubfm {
            dst: MReg::p(PReg::X4),
            rn: MReg::p(PReg::X5),
            immr: 1,
            imms: 63,
            wide: true,
        };
        let ands_br = MOp::LogicalImm {
            dst: MReg::p(PReg::X6),
            rn: MReg::p(PReg::X7),
            imm: 0xff,
            kind: LogicalKind::Ands,
            wide: true,
        };

        // Block 0: `adrp+addlo12`, `adrp+ldr`, `mul -> add` chain, and adjacent `and+cbz`
        // (without the `and+cbz` lock, `and` would hoist to cycle 0 ahead of `add`!).
        func.begin_block(b0);
        func.push_inst(adrp0, MInstFlags::NONE);
        func.push_inst(addlo0, MInstFlags::NONE);
        func.push_inst(adrp1, MInstFlags::NONE);
        func.push_inst(ldr_adrp1, MInstFlags::MEM_READ);
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X12), rn: MReg::p(PReg::X13), rm: MReg::p(PReg::X14), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X12),
                rn: MReg::p(PReg::X12),
                imm12: 5,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(and_cbz, MInstFlags::NONE);
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X1), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b0);

        // Block 1: `ands` separated from `b.eq` by `mul`.
        func.begin_block(b1);
        let i_ands = func.push_inst(ands_br, MInstFlags::NONE);
        func.set_vflag_def(i_ands, 0);
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X8), rn: MReg::p(PReg::X9), rm: MReg::p(PReg::X11), wide: true },
            MInstFlags::NONE,
        );
        let i_beq = func.push_inst(MOp::BCond { cond: MCond::Eq, target: b_exit }, MInstFlags::TERMINATOR);
        func.set_vflag_use(i_beq, 0);
        func.end_block(b1);

        // Block 2: `mul -> add` chain before adjacent `ubfm+cbnz` (without the lock, independent
        // `ubfm` would hoist to cycle 0 ahead of `add` and break `ubfm+cbnz` fusion!).
        let b2 = func.new_block(false);
        func.begin_block(b2);
        func.push_inst(
            MOp::Mul { dst: MReg::p(PReg::X15), rn: MReg::p(PReg::X12), rm: MReg::p(PReg::X8), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X15),
                rn: MReg::p(PReg::X15),
                imm12: 3,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(ubfm_cbnz, MInstFlags::NONE);
        func.push_inst(
            MOp::Cbnz { rt: MReg::p(PReg::X4), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b2);

        func.begin_block(b_exit);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let _stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(mencode(&func, alloc).is_ok());

        // Verify `adrp+addlo12`, `adrp+ldr`, and `and+cbz` stayed strictly glued in `b0`.
        let r0 = func.block_inst_range(b0);
        let ops0 = &func.ops[r0.start..r0.end];
        let mut p_adrp0 = usize::MAX;
        let mut p_addlo0 = usize::MAX;
        let mut p_adrp1 = usize::MAX;
        let mut p_ldr1 = usize::MAX;
        for (rel, &op) in ops0.iter().enumerate() {
            if op == adrp0 {
                p_adrp0 = rel;
            } else if op == addlo0 {
                p_addlo0 = rel;
            } else if op == adrp1 {
                p_adrp1 = rel;
            } else if op == ldr_adrp1 {
                p_ldr1 = rel;
            }
        }
        assert_eq!(p_addlo0, p_adrp0 + 1, "adrp + addlo12 must stay fused");
        assert_eq!(p_ldr1, p_adrp1 + 1, "adrp + ldr [base, #off] must stay fused");
        assert_eq!(ops0[ops0.len() - 2], and_cbz, "adjacent and + cbz must stay fused");

        // Verify `ands` fused immediately before `b.eq` in `b1`.
        let r1 = func.block_inst_range(b1);
        let ops1 = &func.ops[r1.start..r1.end];
        assert_eq!(ops1[ops1.len() - 2], ands_br, "ands + b.eq must fuse");

        // Verify `ubfm` stayed glued immediately before `cbnz` in `b2`.
        let r2 = func.block_inst_range(b2);
        let ops2 = &func.ops[r2.start..r2.end];
        assert_eq!(ops2[ops2.len() - 2], ubfm_cbnz, "adjacent ubfm + cbnz must stay fused");
    }

    #[test]
    fn msched_128_inst_window_hoists_recurrence_past_index_64() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // 78-instruction hot decode loop modeling `decode_exec_leaf_single<1>`:
        //   [0..2]   reads of loop-carried `next_of` (`x19`) into `x3` and `x6`
        //   [2..72]  70 non-critical ALU instructions across 10 independent scratch regs (7-deep chains)
        //   [72..76] critical loop-carried recurrence chain (`mvn x4, x3, lsr #56; lsr x4, x6, x4;
        //            add x4, x4, x2; add x19, x6, x4`) originally stranded past index 64!
        //   [76..78] macro-op fused `subs x0, x0, #1; b.ne b_loop`
        let rec_mvn_op = MOp::Mvn {
            dst: MReg::p(PReg::X4),
            rm: MReg::p(PReg::X3),
            shift: Some(MShift::Lsr(56)),
            wide: true,
        };
        let rec_lsr_op = MOp::ShiftVar {
            dst: MReg::p(PReg::X4),
            rn: MReg::p(PReg::X6),
            rm: MReg::p(PReg::X4),
            kind: ShiftKind::Lsr,
            wide: true,
        };
        let rec_add_op = MOp::AddReg {
            dst: MReg::p(PReg::X4),
            rn: MReg::p(PReg::X4),
            rm: MReg::p(PReg::X2),
            shift: None,
            wide: true,
            sets_flags: false,
        };
        let rec_close_op = MOp::AddReg {
            dst: MReg::p(PReg::X19),
            rn: MReg::p(PReg::X6),
            rm: MReg::p(PReg::X4),
            shift: None,
            wide: true,
            sets_flags: false,
        };
        let subs_op = MOp::SubImm {
            dst: MReg::p(PReg::X0),
            rn: MReg::p(PReg::X0),
            imm12: 1,
            shift12: false,
            wide: true,
            sets_flags: true,
        };

        func.begin_block(b_loop);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X3),
                rn: MReg::p(PReg::X19),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X6),
                rn: MReg::p(PReg::X19),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );

        let scratch_regs = [
            PReg::X5,
            PReg::X7,
            PReg::X8,
            PReg::X9,
            PReg::X10,
            PReg::X11,
            PReg::X12,
            PReg::X13,
            PReg::X14,
            PReg::X15,
        ];
        // 70 independent non-critical ALU instructions (indices 2..72).
        for step in 0..70usize {
            let reg = scratch_regs[step % scratch_regs.len()];
            let rn = if step < scratch_regs.len() { PReg::X1 } else { reg };
            let imm12 = u16::try_from(step + 1).unwrap_or(1);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::p(reg),
                    rn: MReg::p(rn),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }

        // Critical loop-carried recurrence chain placed at indices 72..76.
        func.push_inst(rec_mvn_op, MInstFlags::NONE);
        func.push_inst(rec_lsr_op, MInstFlags::NONE);
        func.push_inst(rec_add_op, MInstFlags::NONE);
        func.push_inst(rec_close_op, MInstFlags::NONE);

        // Trailing macro-op fused loop latch (indices 76..78).
        let i_subs = func.push_inst(subs_op, MInstFlags::NONE);
        func.set_vflag_def(i_subs, 0);
        let i_br = func.push_inst(MOp::BCond { cond: MCond::Ne, target: b_loop }, MInstFlags::TERMINATOR);
        func.set_vflag_use(i_br, 0);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X19), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        let before = func.block_inst_range(b_loop);
        assert_eq!(before.end - before.start, 78, "expected exact 78-inst loop block");
        assert_eq!(func.ops[before.start + 72], rec_mvn_op);
        assert_eq!(func.ops[before.start + 73], rec_lsr_op);
        assert_eq!(func.ops[before.start + 74], rec_add_op);
        assert_eq!(func.ops[before.start + 75], rec_close_op);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert_eq!(
            stats.regions_scheduled, 1,
            "entire 77-inst barrier-free loop body must schedule as one 128-inst window: {stats:?}"
        );
        assert!(stats.recurrences_boosted >= 2, "expected x19 + x0 recurrences boosted: {stats:?}");
        assert!(stats.insts_reordered > 0, "expected instructions reordered: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));
        assert!(mencode(&func, alloc).is_ok(), "scheduled 78-inst loop must encode cleanly");

        let range = func.block_inst_range(b_loop);
        let ops = &func.ops[range.start..range.end];
        let mut pos_mvn = usize::MAX;
        let mut pos_lsr = usize::MAX;
        let mut pos_add = usize::MAX;
        let mut pos_close = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == rec_mvn_op {
                pos_mvn = rel;
            } else if op == rec_lsr_op {
                pos_lsr = rel;
            } else if op == rec_add_op {
                pos_add = rel;
            } else if op == rec_close_op {
                pos_close = rel;
            }
        }
        assert!(pos_mvn < pos_lsr && pos_lsr < pos_add && pos_add < pos_close);
        assert!(
            pos_close < ops.len() / 2,
            "128-inst window must hoist recurrence chain from indices 72..76 into first half (< 39): mvn={pos_mvn}, lsr={pos_lsr}, add={pos_add}, close={pos_close}"
        );
        assert_eq!(ops[ops.len() - 2], subs_op, "subs must remain macro-op fused before b.ne");
    }

    #[test]
    fn msched_pre_ra_schedules_base_mem_op_before_ptr_step_add_imm() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let v_ptr = func.alloc_vreg(VRegClass::Gpr64);
        let v_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_end = func.alloc_vreg(VRegClass::Gpr64);

        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        let add_step_op = MOp::AddImm {
            dst: MReg::V(v_next),
            rn: MReg::V(v_ptr),
            imm12: 32,
            shift12: false,
            wide: true,
            sets_flags: false,
        };
        let stp_op = MOp::Stp {
            src0: MReg::vec(16),
            src1: MReg::vec(16),
            base: MReg::V(v_ptr),
            off: 0,
            size: MemSize::Q128,
            post_idx: false,
            pre_idx: false,
        };
        let cmp_op = MOp::CmpReg {
            rn: MReg::V(v_next),
            rm: MReg::V(v_end),
            shift: None,
            wide: true,
        };

        // Reversed input order: `v_next = add v_ptr, #32` (boosted recurrence!) placed BEFORE
        // `stp q16, q16, [v_ptr]`. Pre-RA scheduling must emit `Stp` first so `v_ptr` dies at
        // `AddImm` without overlapping `v_next` (enabling `mra` coalescing + post-index fusion).
        func.begin_block(b_loop);
        func.push_inst(add_step_op, MInstFlags::NONE);
        func.push_inst(stp_op, MInstFlags::MEM_WRITE);
        let i_cmp = func.push_inst(cmp_op, MInstFlags::NONE);
        func.set_vflag_def(i_cmp, 0);
        func.push_parallel_copy(&[(MReg::V(v_ptr), MReg::V(v_next))], MInstFlags::NONE);
        let i_br = func.push_inst(
            MOp::BCond { cond: MCond::Lo, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(i_br, 0);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert!(stats.recurrences_boosted >= 1, "expected ptr step recurrence boosted: {stats:?}");
        assert!(stats.insts_reordered > 0, "expected Stp and AddImm to reorder: {stats:?}");
        assert_eq!(mverify(&func), Ok(()));

        let range = func.block_inst_range(b_loop);
        let ops = &func.ops[range.start..range.end];
        let mut pos_stp = usize::MAX;
        let mut pos_add = usize::MAX;
        let mut pos_cmp = usize::MAX;
        for (rel, &op) in ops.iter().enumerate() {
            if op == stp_op {
                pos_stp = rel;
            } else if op == add_step_op {
                pos_add = rel;
            } else if op == cmp_op {
                pos_cmp = rel;
            }
        }
        assert!(
            pos_stp < pos_add,
            "pre-RA msched must emit Stp reading v_ptr before AddImm defining v_next: stp={pos_stp}, add={pos_add}"
        );
        assert!(
            pos_add < pos_cmp,
            "AddImm defining v_next must precede CmpReg reading v_next: add={pos_add}, cmp={pos_cmp}"
        );
    }

    #[test]
    fn msched_pre_ra_defers_add_reg_ptr_step_until_unscheduled_store_issues() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let v_ptr = func.alloc_vreg(VRegClass::Gpr64);
        let v_off = func.alloc_vreg(VRegClass::Gpr64);
        let v_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_a = func.alloc_vreg(VRegClass::Gpr64);
        let v_b = func.alloc_vreg(VRegClass::Gpr64);
        let v_val = func.alloc_vreg(VRegClass::Gpr64);

        let b0 = func.new_block(false);
        func.begin_block(b0);
        func.push_inst(MOp::Mov { dst: MReg::V(v_ptr), src: MReg::p(PReg::X0), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Mov { dst: MReg::V(v_off), src: MReg::p(PReg::X1), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Mov { dst: MReg::V(v_a), src: MReg::p(PReg::X2), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Mov { dst: MReg::V(v_b), src: MReg::p(PReg::X3), wide: true }, MInstFlags::NONE);

        // Input order has `v_next = add v_ptr, v_off` ready immediately, BEFORE `v_val = orr v_a, v_b`
        // and `str v_val, [v_ptr]` (so `str` is NOT ready when `add` first becomes ready!).
        let add_step_op = MOp::AddReg {
            dst: MReg::V(v_next),
            rn: MReg::V(v_ptr),
            rm: MReg::V(v_off),
            shift: None,
            wide: true,
            sets_flags: false,
        };
        let orr_op = MOp::LogicalReg {
            dst: MReg::V(v_val),
            rn: MReg::V(v_a),
            rm: MReg::V(v_b),
            shift: None,
            kind: LogicalKind::Orr,
            invert_rm: false,
            wide: true,
        };
        let str_op = MOp::Str {
            src: MReg::V(v_val),
            addr: AddrMode::BaseOff { base: MReg::V(v_ptr), off: 0 },
            size: MemSize::X64,
        };
        func.push_inst(add_step_op, MInstFlags::NONE);
        func.push_inst(orr_op, MInstFlags::NONE);
        func.push_inst(str_op, MInstFlags::MEM_WRITE);
        func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_next), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let _ = schedule_mfunc(&mut func, TargetModel::APPLE_M5, alloc);
        assert_eq!(mverify(&func), Ok(()));

        let range = func.block_inst_range(b0);
        let ops = &func.ops[range.start..range.end];
        let pos_str = ops.iter().position(|&op| op == str_op).expect("str present");
        let pos_add = ops.iter().position(|&op| op == add_step_op).expect("add present");
        assert!(
            pos_str < pos_add,
            "pre-RA msched must defer AddReg ptr step behind unscheduled Str [v_ptr]: str={pos_str}, add={pos_add}"
        );
    }
}

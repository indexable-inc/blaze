#![cfg_attr(not(test), forbid(unsafe_code))]
//! Typed SSA & Post-RA AArch64 Machine IR Optimizer (`mopt`) for `TierName::Sota` (V3 Wave 2).
//!
//! Replaces the legacy `u32`-disassembling passes of `mach_a64.rs` with provably sound, typed
//! passes operating directly over [`super::mfunc::MFunc`] (zero `u32` disassembly):
//! - Works identically on pre-RA SSA functions (`func.is_ssa == true`, `VReg`s) and post-RA
//!   physical functions (`func.is_ssa == false`, `PReg`s).
//! - Never mutates or deletes instructions carrying [`MInstFlags::PINNED`],
//!   [`MInstFlags::VOLATILE`], or non-empty `note_idx` chains.
//! - Preserves all [`super::mfunc::mverify`] structural, operand-role (`SP` vs `XZR`), platform
//!   ABI (`X18` on macOS), and single-live-`VFlag` invariants across every pass.
//!
//! # Passes
//! 0. [`cse_global_multi_inst_mov_imm`] & [`mopt_dead_narrow32_mov_ssa`] (SSA only): global
//!    entry-hoisted CSE for multi-instruction `MovImm` (`cost >= 2`) and whole-function dead
//!    32-bit truncation `Mov { wide: false }` elision.
//! 1. [`mopt_peepholes_and_cse`]: self-`Mov` removal, known-zero-upper-32 tracking, post-RA dead
//!    `mov wR, wR` truncation elision, intra-MBB `MovImm` CSE, local wide and narrow-32 `Mov` copy
//!    propagation, SSA `cset` + `AddReg` -> `Csinc` (`cinc`) fusion, `SubImm #1` + `Csinc` (CINC)
//!    commutation + `CmpImm #0` deletion, `SubImm`-into-`CmpImm` fusion (only when `dst != rn`),
//!    `Add`/`Sub` + `CmpImm #0` flag fusion (N/Z-only), `Ubfm` LSR + UBFX fusion, single-use
//!    `Ubfm`/`Sbfm` shift-into-ALU/`Mvn` fusion, and SSA `neg` (optional shift) + `AddReg` ->
//!    `SubReg` fusion.
//! 2. [`mopt_redundant_loads`]: zero-allocation (`32`-slot stack table) intra-MBB redundant load
//!    elimination & store-to-load forwarding (including post-RA `[SP, #off]` spill slots and `Mov`
//!    alias tracking) with exact same-base non-overlapping alias checks and conservative
//!    invalidation on other-base stores, `SP` writes, barriers, calls, `RawWord`s, and volatiles.
//! 3. [`mopt_pair_ldp_stp_and_post_index`]: intra-MBB `Ldp`/`Stp` pairing within a 12-instruction
//!    sliding window and zero-offset `Ldr`/`Str`/`Ldp`/`Stp` + `AddImm`/`SubImm` post-index fusion.
//! 4. [`mopt_branch_opt`]: bounded empty-trampoline branch threading (`<= 16` hops), conditional
//!    branch inversion over trailing `B` (including across empty/pass-through fallthrough chains),
//!    fallthrough `B` elision, unreachable empty/trampoline block pruning, post-RA identical
//!    short-block tail merging up to 12 ops (Pass 4e), post-RA distant / post-`Ret` copy-trampoline
//!    hoisting (Pass 4g), short edge-split block relocation & distant back-edge trampoline
//!    inversion/co-location (Pass 4d Phases 1–3), post-RA short hot loop-header relocation before
//!    its fallthrough-free body (Pass 4f), and `CFG` rebuild.
//! 5. [`mopt_fuse_addsub_cbz_post_ra`] (post-RA only): fuses adjacent `SubImm`/`AddImm`/`SubReg`/
//!    `AddReg` + `Cbz`/`Cbnz` into Apple Silicon macro-op fused `subs`/`adds` + `b.eq`/`b.ne`.

use blaze_own::{Alloc, Vec};

use crate::enc;

use super::mfunc::{
    AddrMode, FpAluOp, LogicalKind, MBlock, MCond, MExt, MFunc, MInstFlags, MNoteKind, MOp, MReg,
    MShift, MemSize, NO_NOTE, NO_VFLAG, PReg, VReg, VRegClass, VecAluOp,
};

/// Sliding window bound for Pass 1 intra-MBB peepholes and copy propagation (Rule 18).
const PEEPHOLE_WINDOW: usize = 24;

/// Sliding window bound for Pass 3 `Ldp`/`Stp` pairing (Rule 18).
const PAIR_WINDOW: usize = 12;

/// Sliding window bound for Pass 3 post-index writeback fusion (Rule 18).
const POST_IDX_WINDOW: usize = 64;

/// Fixed stack table capacity for Pass 2 intra-MBB load/store forwarding (zero allocation).
const LOAD_SLOTS: usize = 32;

/// Fixed stack table capacity for Pass 1 intra-MBB `MovImm` CSE (zero allocation).
const IMM_CSE_SLOTS: usize = 16;

/// Fixed stack table capacity for Pass 0a global multi-instruction `MovImm` CSE (Rule 18).
const GLOBAL_IMM_TABLE_CAP: usize = 16;

/// Maximum multi-instruction `MovImm` constants hoisted to `entry_blk` per SSA function.
const MAX_HOISTED_IMMS: usize = 4;

/// Maximum trampoline-threading hops in Pass 4 (bounded progress, Rule 18).
const MAX_THREAD_HOPS: usize = 16;

/// Maximum short edge-split block relocations (`S -> B T`) per Pass 4 call (Rule 18).
const MAX_BLOCK_RELOCATIONS: usize = 32;

/// Maximum non-NOP instruction count of a short edge-split block eligible for relocation.
const MAX_RELOCATE_BLOCK_INSTS: usize = 6;

/// Maximum non-NOP instruction count of a pure fallthrough copy block `P` before `T` eligible for
/// secondary I-cache co-location of `S -> B T` immediately before `P`.
const MAX_SECONDARY_CO_LOCATE_P_INSTS: usize = 4;

/// Maximum total non-NOP instruction count (`1..=12`, including trailing `B`) for Pass 4e post-RA
/// identical short-block tail merging.
const MAX_TAIL_MERGE_OPS: usize = 12;

/// Maximum prefix instruction count (`0..=11`, i.e. `1..=12` total ops with trailing `B`, or
/// `1..=11` ops for a pure fallthrough block immediately before `target`) for Pass 4e post-RA
/// identical short-block tail merging.
const MAX_TAIL_MERGE_PREFIX_OPS: usize = MAX_TAIL_MERGE_OPS - 1;

/// Maximum candidate blocks compared per target bucket in Pass 4e (Rule 18).
const MAX_TAIL_MERGE_BUCKET: usize = 64;

/// Maximum non-NOP instruction count (`1..=10`, all non-terminator ops `Mov`/`MovImm`, ending in
/// `MOp::B { target }`) for Pass 4g post-RA distant / post-`Ret` copy-trampoline hoisting.
const MAX_TRAMPOLINE_HOIST_INSTS: usize = 10;

/// Minimum `block_order` distance (`s_pos > t_pos + 8`) for Pass 4g hoisting when no `Ret` sits
/// between `T` and `S`.
const MIN_TRAMPOLINE_HOIST_DIST: usize = 8;

/// Maximum distant / post-`Ret` copy-trampoline hoists per Pass 4g call (Rule 18).
const MAX_TRAMPOLINE_HOISTS: usize = 32;

/// Bitmask covering all 31 architectural GPRs (`x0..=x30`) for post-RA bitmask liveness.
const ALL_GPRS_MASK: u32 = 0x7fff_ffff;

/// Maximum backward fixed-point iterations for post-RA GPR bitmask liveness (Rule 18).
const MAX_LIVENESS_ITERS: usize = 32;

/// Known-immediate slot count per block for post-RA cross-block `MovImm` elision (Rule 18).
const CROSS_BLOCK_IMM_SLOTS: usize = 8;

/// Maximum non-NOP instruction count of a short loop-header block `H` eligible for Pass 4f.
const MAX_HEADER_RELOC_INSTS: usize = 8;

/// Maximum hot loop-header relocations per Pass 4f call (Rule 18).
const MAX_HEADER_RELOCATIONS: usize = 32;

/// Maximum non-NOP instruction count of a distant back-edge trampoline `S -> B T` in Pass 4d Phase 3.
const MAX_BACKEDGE_SPLIT_INSTS: usize = 3;

// -----------------------------------------------------------------------------
// Zero-`as` conversion helpers
// -----------------------------------------------------------------------------

#[inline]
const fn chunk16(v: u64, hw: u32) -> u32 {
    let shifted = v >> (hw * 16);
    let b = shifted.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], 0, 0])
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(v: usize) -> Option<u32> {
    u32::try_from(v).ok()
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
fn op_at(func: &MFunc<'_>, idx: usize) -> MOp {
    func.ops.get(idx).copied().unwrap_or(MOp::Nop)
}

#[inline]
fn flags_at(func: &MFunc<'_>, idx: usize) -> MInstFlags {
    func.flags.get(idx).copied().unwrap_or(MInstFlags::NONE)
}

#[inline]
fn vflag_def_at(func: &MFunc<'_>, idx: usize) -> u32 {
    func.vflag_def.get(idx).copied().unwrap_or(NO_VFLAG)
}

#[inline]
fn vflag_use_at(func: &MFunc<'_>, idx: usize) -> u32 {
    func.vflag_use.get(idx).copied().unwrap_or(NO_VFLAG)
}

#[inline]
fn note_at(func: &MFunc<'_>, idx: usize) -> u32 {
    func.note_idx.get(idx).copied().unwrap_or(NO_NOTE)
}

#[inline]
fn block_range(func: &MFunc<'_>, blk: MBlock) -> (usize, usize) {
    let Some(&(start_u, end_u)) = func.block_insts.get(to_idx(blk.0)) else {
        return (0, 0);
    };
    (to_idx(start_u), to_idx(end_u).min(func.ops.len()))
}

#[inline]
fn copy_inst_slot(func: &mut MFunc<'_>, dst: usize, src: usize) {
    let op = op_at(func, src);
    let fl = flags_at(func, src);
    let vd = vflag_def_at(func, src);
    let vu = vflag_use_at(func, src);
    let nt = note_at(func, src);
    set(&mut func.ops, dst, op);
    set(&mut func.flags, dst, fl);
    set(&mut func.vflag_def, dst, vd);
    set(&mut func.vflag_use, dst, vu);
    set(&mut func.note_idx, dst, nt);
}

// -----------------------------------------------------------------------------
// Public statistics & entry point
// -----------------------------------------------------------------------------

/// Aggregate optimization counters returned by [`optimize_mfunc`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MOptStats {
    pub peepholes: u32,
    pub movs_eliminated: u32,
    pub loads_forwarded: u32,
    pub pairs_formed: u32,
    pub post_idx_formed: u32,
    pub branches_folded: u32,
    pub trampolines_duped: u32,
}

/// Runs all typed AArch64 machine IR optimization passes in place on `func` (both SSA pre-RA
/// and physical post-RA), preserving every [`super::mfunc::mverify`] invariant and side-band
/// [`super::mfunc::MNote`].
pub fn optimize_mfunc<'a>(func: &mut MFunc<'a>, alloc: Alloc<'a>) -> MOptStats {
    let mut stats = MOptStats::default();
    if func.ops.is_empty() || func.block_order.is_empty() || func.block_insts.is_empty() {
        return stats;
    }

    // Pass 0 (SSA only): Global entry-hoisted multi-instruction `MovImm` CSE & whole-function
    // dead 32-bit truncation `Mov { wide: false }` elision.
    if func.is_ssa {
        cse_global_multi_inst_mov_imm(func, alloc, &mut stats);
        compact_nops(func, alloc);
        mopt_dead_narrow32_mov_ssa(func, alloc, &mut stats);
        compact_nops(func, alloc);
    }

    // Pass 1: Typed intra-MBB peepholes, `MovImm` CSE, copy propagation, flag/bitfield fusion.
    mopt_peepholes_and_cse(func, alloc, &mut stats);
    compact_nops(func, alloc);

    // Pass 1m:
    // - SSA: global cross-block pure `Mov { wide: true }` copy elimination (folds `u32 as usize`
    //   copies upgraded by `peephole_known_zero_upper32` across all basic blocks).
    // - Post-RA: cross-block known-`MovImm` elision over CFG predecessors.
    if func.is_ssa {
        mopt_global_wide_mov_ssa(func, alloc, &mut stats);
        compact_nops(func, alloc);
    } else {
        mopt_cross_block_mov_imm_post_ra(func, alloc, &mut stats);
        compact_nops(func, alloc);
    }

    // Pass 2: Zero-allocation intra-MBB redundant load elimination & store-to-load forwarding.
    let loads_before = stats.loads_forwarded;
    mopt_redundant_loads(func, &mut stats);
    compact_nops(func, alloc);
    if stats.loads_forwarded > loads_before {
        mopt_post_load_forward_cleanup(func, alloc, &mut stats);
        compact_nops(func, alloc);
    }

    // Pass 3: Intra-MBB `Ldp`/`Stp` pairing and zero-offset post-index writeback fusion.
    mopt_pair_ldp_stp_and_post_index(func, alloc, &mut stats);
    compact_nops(func, alloc);

    // Pass 4: Trampoline threading, conditional branch inversion, fallthrough `B` elision,
    // short edge-split block relocation, unreachable empty block pruning, and CFG rebuild.
    mopt_branch_opt(func, alloc, &mut stats);

    // Pass 5 (post-RA only): Apple Silicon macro-op fusion `add/sub + cbz/cbnz` -> `adds/subs + b.eq/b.ne`.
    if !func.is_ssa {
        mopt_fuse_addsub_cbz_post_ra(func, &mut stats);
    }

    stats
}

// -----------------------------------------------------------------------------
// Typed `MOp` / `MFunc` analysis helpers (zero `u32` disassembly)
// -----------------------------------------------------------------------------

#[inline]
fn has_volatile_note(func: &MFunc<'_>, idx: usize) -> bool {
    let n_notes = func.notes.len();
    let mut cur = func.note_idx.get(idx).copied().unwrap_or(NO_NOTE);
    let mut steps: usize = 0;
    while cur != NO_NOTE && steps < n_notes {
        let ci = to_idx(cur);
        let Some(&note) = func.notes.get(ci) else {
            break;
        };
        if matches!(note.kind, MNoteKind::Volatile) {
            return true;
        }
        cur = note.next;
        steps += 1;
    }
    false
}

#[inline]
fn is_volatile_inst(func: &MFunc<'_>, idx: usize) -> bool {
    let fl = func.flags.get(idx).copied().unwrap_or(MInstFlags::NONE);
    fl.contains(MInstFlags::VOLATILE) || has_volatile_note(func, idx)
}

/// True iff instruction `idx` must not be deleted or mutated (`PINNED`, `VOLATILE`, or noted).
#[inline]
fn is_protected(func: &MFunc<'_>, idx: usize) -> bool {
    let fl = func.flags.get(idx).copied().unwrap_or(MInstFlags::NONE);
    let note = func.note_idx.get(idx).copied().unwrap_or(NO_NOTE);
    fl.contains(MInstFlags::PINNED) || fl.contains(MInstFlags::VOLATILE) || note != NO_NOTE
}

/// Replaces a non-protected instruction at `idx` with a clean removable `MOp::Nop`.
#[inline]
fn mark_deleted(func: &mut MFunc<'_>, idx: usize) {
    if idx < func.ops.len() {
        set(&mut func.ops, idx, MOp::Nop);
        set(&mut func.flags, idx, MInstFlags::NONE);
        set(&mut func.vflag_def, idx, NO_VFLAG);
        set(&mut func.vflag_use, idx, NO_VFLAG);
    }
}

/// True if `op` transfers control flow or invokes a callee.
#[inline]
const fn is_control_or_call(op: &MOp) -> bool {
    matches!(
        *op,
        MOp::Bl
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
            | MOp::Udf
    )
}

/// True if `idx` is a call, memory barrier, trap-capable barrier, or opaque barrier `RawWord`.
#[inline]
fn is_call_or_mem_barrier(func: &MFunc<'_>, idx: usize) -> bool {
    let fl = func.flags.get(idx).copied().unwrap_or(MInstFlags::NONE);
    if fl.contains(MInstFlags::CALL)
        || fl.contains(MInstFlags::MEM_BARRIER)
        || fl.contains(MInstFlags::VOLATILE)
    {
        return true;
    }
    match func.ops.get(idx) {
        Some(MOp::Bl | MOp::Blr { .. } | MOp::Brk { .. } | MOp::Udf) => true,
        Some(MOp::RawWord { is_mem_barrier: true, .. }) => true,
        _ => has_volatile_note(func, idx),
    }
}

/// True if `reg` is a real register (not `XZR` / `Nzcv`).
#[inline]
const fn is_trackable_reg(reg: MReg) -> bool {
    !matches!(reg, MReg::P(PReg::Xzr | PReg::Nzcv))
}

/// True if `reg` is `SP` or `XZR` (special hardware encoding 31).
#[inline]
const fn is_sp_or_xzr(reg: MReg) -> bool {
    matches!(reg, MReg::P(PReg::Sp | PReg::Xzr))
}

#[inline]
const fn raw_mask_hits_reg(gpr_mask: u32, fp_mask: u32, reg: MReg) -> bool {
    match reg {
        MReg::P(PReg::Gpr(r)) if r <= 30 => (gpr_mask & (1u32 << r)) != 0,
        MReg::P(PReg::Sp) => (gpr_mask & (1u32 << 31)) != 0,
        MReg::P(PReg::V(r)) if r <= 31 => (fp_mask & (1u32 << r)) != 0,
        MReg::P(PReg::Gpr(_) | PReg::V(_) | PReg::Xzr | PReg::Nzcv) => false,
        MReg::V(_) => (gpr_mask | fp_mask) != 0,
    }
}

/// Exhaustive typed check: true iff instruction `idx` may write `reg`.
fn inst_defines_reg(func: &MFunc<'_>, idx: usize, reg: MReg) -> bool {
    if !is_trackable_reg(reg) {
        return false;
    }
    let Some(op) = func.ops.get(idx) else {
        return false;
    };
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
        | MOp::AdrTable { dst, .. } => dst == reg,
        MOp::Ldr { dst, addr, .. } => {
            if dst == reg {
                return true;
            }
            match addr {
                AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => base == reg,
                AddrMode::BaseOff { .. } | AddrMode::ScaledIdx { .. } => false,
            }
        }
        MOp::Str { addr, .. } => match addr {
            AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => base == reg,
            AddrMode::BaseOff { .. } | AddrMode::ScaledIdx { .. } => false,
        },
        MOp::Ldp { dst0, dst1, base, post_idx, pre_idx, .. } => {
            dst0 == reg || dst1 == reg || ((post_idx || pre_idx) && base == reg)
        }
        MOp::Stp { base, post_idx, pre_idx, .. } => (post_idx || pre_idx) && base == reg,
        MOp::CmpImm { .. }
        | MOp::CmpReg { .. }
        | MOp::TstImm { .. }
        | MOp::TstReg { .. }
        | MOp::CcmpImm { .. }
        | MOp::CcmpReg { .. }
        | MOp::FpCmp { .. }
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
        | MOp::Nop => false,
        // Calls conservatively clobber all non-SP registers in intra-MBB analysis.
        MOp::Bl | MOp::Blr { .. } => reg != MReg::SP,
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(func.copy_pairs.len());
            if let Some(pairs) = func.copy_pairs.get(s..e) {
                for &(dst, _) in pairs {
                    if dst == reg {
                        return true;
                    }
                }
            }
            false
        }
        MOp::RawWord { defs_gpr, defs_fp, .. } => raw_mask_hits_reg(defs_gpr, defs_fp, reg),
    }
}

/// Exhaustive typed check: true iff instruction `idx` may read `reg`.
fn inst_uses_reg(func: &MFunc<'_>, idx: usize, reg: MReg) -> bool {
    if !is_trackable_reg(reg) {
        return false;
    }
    let Some(op) = func.ops.get(idx) else {
        return false;
    };
    match *op {
        MOp::Mov { src, .. } | MOp::MovSp { src, .. } => src == reg,
        MOp::MovImm { .. }
        | MOp::Adrp { .. }
        | MOp::AdrTable { .. }
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop => false,
        MOp::AddImm { rn, .. }
        | MOp::SubImm { rn, .. }
        | MOp::LogicalImm { rn, .. }
        | MOp::Ubfm { rn, .. }
        | MOp::Sbfm { rn, .. }
        | MOp::Rbit { rn, .. }
        | MOp::Clz { rn, .. }
        | MOp::Rev { rn, .. }
        | MOp::CmpImm { rn, .. }
        | MOp::TstImm { rn, .. }
        | MOp::CcmpImm { rn, .. }
        | MOp::AddLo12 { rn, .. } => rn == reg,
        MOp::Mvn { rm, .. } => rm == reg,
        MOp::Bfxil { dst, rn, .. } => dst == reg || rn == reg,
        MOp::AddReg { rn, rm, .. }
        | MOp::SubReg { rn, rm, .. }
        | MOp::AddExt { rn, rm, .. }
        | MOp::SubExt { rn, rm, .. }
        | MOp::LogicalReg { rn, rm, .. }
        | MOp::ShiftVar { rn, rm, .. }
        | MOp::Extr { rn, rm, .. }
        | MOp::Mul { rn, rm, .. }
        | MOp::Umulh { rn, rm, .. }
        | MOp::Smulh { rn, rm, .. }
        | MOp::CarryAddSub { rn, rm, .. }
        | MOp::Udiv { rn, rm, .. }
        | MOp::Sdiv { rn, rm, .. }
        | MOp::Umin { rn, rm, .. }
        | MOp::Umax { rn, rm, .. }
        | MOp::Smin { rn, rm, .. }
        | MOp::Smax { rn, rm, .. }
        | MOp::CmpReg { rn, rm, .. }
        | MOp::TstReg { rn, rm, .. }
        | MOp::CcmpReg { rn, rm, .. }
        | MOp::Csel { rn, rm, .. }
        | MOp::Csinc { rn, rm, .. }
        | MOp::Csinv { rn, rm, .. }
        | MOp::Csneg { rn, rm, .. }
        | MOp::FpCmp { rn, rm, .. }
        | MOp::Fcsel { rn, rm, .. } => rn == reg || rm == reg,
        MOp::Madd { rn, rm, ra, .. } | MOp::Msub { rn, rm, ra, .. } => {
            rn == reg || rm == reg || ra == reg
        }
        MOp::FpAlu { rn, rm, ra, .. } => rn == reg || rm == Some(reg) || ra == Some(reg),
        MOp::VecAlu { op: vop, dst, rn, rm, .. } => match vop {
            VecAluOp::Zero => false,
            VecAluOp::Bsl
            | VecAluOp::Bit
            | VecAluOp::Bif
            | VecAluOp::Mla
            | VecAluOp::Mls
            | VecAluOp::Fmla
            | VecAluOp::Fmls => dst == reg || rn == reg || rm == Some(reg),
            _ => rn == reg || rm == Some(reg),
        },
        MOp::Ldr { addr, .. } => addr_uses_reg(addr, reg),
        MOp::Str { src, addr, .. } => src == reg || addr_uses_reg(addr, reg),
        MOp::Ldp { base, .. } | MOp::Prfm { base, .. } => base == reg,
        MOp::Stp { src0, src1, base, .. } => src0 == reg || src1 == reg || base == reg,
        MOp::Cbz { rt, .. }
        | MOp::Cbnz { rt, .. }
        | MOp::Tbz { rt, .. }
        | MOp::Tbnz { rt, .. } => rt == reg,
        // Calls, indirect branches, and returns conservatively read live ABI/argument registers.
        MOp::Bl | MOp::Blr { .. } | MOp::Br { .. } => true,
        MOp::Ret => match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => (func.ret_live_gprs & (1u32 << r)) != 0 || r == 30,
            MReg::P(PReg::Sp) => true,
            MReg::P(PReg::V(_)) => true,
            MReg::V(_) => true,
            MReg::P(PReg::Gpr(_) | PReg::Xzr | PReg::Nzcv) => false,
        },
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(func.copy_pairs.len());
            if let Some(pairs) = func.copy_pairs.get(s..e) {
                for &(_, src) in pairs {
                    if src == reg {
                        return true;
                    }
                }
            }
            false
        }
        MOp::RawWord { uses_gpr, uses_fp, .. } => raw_mask_hits_reg(uses_gpr, uses_fp, reg),
    }
}

#[inline]
fn addr_uses_reg(addr: AddrMode, reg: MReg) -> bool {
    match addr {
        AddrMode::BaseOff { base, .. }
        | AddrMode::PostIdx { base, .. }
        | AddrMode::PreIdx { base, .. } => base == reg,
        AddrMode::ScaledIdx { base, idx, .. } => base == reg || idx == reg,
    }
}

#[inline]
fn inst_touches_reg(func: &MFunc<'_>, idx: usize, reg: MReg) -> bool {
    inst_defines_reg(func, idx, reg) || inst_uses_reg(func, idx, reg)
}

/// Visits every `VReg` read by `op` (for exact SSA use counting).
fn visit_op_vreg_uses(op: &MOp, copy_pairs: &[(MReg, MReg)], mut f: impl FnMut(usize)) {
    let mut reg = |r: MReg| {
        if let MReg::V(v) = r {
            f(to_idx(v.0));
        }
    };
    match *op {
        MOp::Mov { src, .. } | MOp::MovSp { src, .. } => reg(src),
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
        MOp::Blr { target } | MOp::Br { target } => reg(target),
        MOp::AddImm { rn, .. }
        | MOp::SubImm { rn, .. }
        | MOp::LogicalImm { rn, .. }
        | MOp::Ubfm { rn, .. }
        | MOp::Sbfm { rn, .. }
        | MOp::Rbit { rn, .. }
        | MOp::Clz { rn, .. }
        | MOp::Rev { rn, .. }
        | MOp::CmpImm { rn, .. }
        | MOp::TstImm { rn, .. }
        | MOp::CcmpImm { rn, .. }
        | MOp::AddLo12 { rn, .. } => reg(rn),
        MOp::Mvn { rm, .. } => reg(rm),
        MOp::Bfxil { dst, rn, .. } => {
            reg(dst);
            reg(rn);
        }
        MOp::AddReg { rn, rm, .. }
        | MOp::SubReg { rn, rm, .. }
        | MOp::AddExt { rn, rm, .. }
        | MOp::SubExt { rn, rm, .. }
        | MOp::LogicalReg { rn, rm, .. }
        | MOp::ShiftVar { rn, rm, .. }
        | MOp::Extr { rn, rm, .. }
        | MOp::Mul { rn, rm, .. }
        | MOp::Umulh { rn, rm, .. }
        | MOp::Smulh { rn, rm, .. }
        | MOp::CarryAddSub { rn, rm, .. }
        | MOp::Udiv { rn, rm, .. }
        | MOp::Sdiv { rn, rm, .. }
        | MOp::Umin { rn, rm, .. }
        | MOp::Umax { rn, rm, .. }
        | MOp::Smin { rn, rm, .. }
        | MOp::Smax { rn, rm, .. }
        | MOp::CmpReg { rn, rm, .. }
        | MOp::TstReg { rn, rm, .. }
        | MOp::CcmpReg { rn, rm, .. }
        | MOp::Csel { rn, rm, .. }
        | MOp::Csinc { rn, rm, .. }
        | MOp::Csinv { rn, rm, .. }
        | MOp::Csneg { rn, rm, .. }
        | MOp::FpCmp { rn, rm, .. }
        | MOp::Fcsel { rn, rm, .. } => {
            reg(rn);
            reg(rm);
        }
        MOp::Madd { rn, rm, ra, .. } | MOp::Msub { rn, rm, ra, .. } => {
            reg(rn);
            reg(rm);
            reg(ra);
        }
        MOp::FpAlu { rn, rm, ra, .. } => {
            reg(rn);
            if let Some(m) = rm {
                reg(m);
            }
            if let Some(a) = ra {
                reg(a);
            }
        }
        MOp::VecAlu { op: vop, dst, rn, rm, .. } => match vop {
            VecAluOp::Zero => {}
            VecAluOp::Bsl
            | VecAluOp::Bit
            | VecAluOp::Bif
            | VecAluOp::Mla
            | VecAluOp::Mls
            | VecAluOp::Fmla
            | VecAluOp::Fmls => {
                reg(dst);
                reg(rn);
                if let Some(m) = rm {
                    reg(m);
                }
            }
            _ => {
                reg(rn);
                if let Some(m) = rm {
                    reg(m);
                }
            }
        },
        MOp::Ldr { addr, .. } => match addr {
            AddrMode::BaseOff { base, .. }
            | AddrMode::PostIdx { base, .. }
            | AddrMode::PreIdx { base, .. } => reg(base),
            AddrMode::ScaledIdx { base, idx, .. } => {
                reg(base);
                reg(idx);
            }
        },
        MOp::Str { src, addr, .. } => {
            reg(src);
            match addr {
                AddrMode::BaseOff { base, .. }
                | AddrMode::PostIdx { base, .. }
                | AddrMode::PreIdx { base, .. } => reg(base),
                AddrMode::ScaledIdx { base, idx, .. } => {
                    reg(base);
                    reg(idx);
                }
            }
        }
        MOp::Ldp { base, .. } | MOp::Prfm { base, .. } => reg(base),
        MOp::Stp { src0, src1, base, .. } => {
            reg(src0);
            reg(src1);
            reg(base);
        }
        MOp::Cbz { rt, .. }
        | MOp::Cbnz { rt, .. }
        | MOp::Tbz { rt, .. }
        | MOp::Tbnz { rt, .. } => reg(rt),
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(copy_pairs.len());
            if let Some(pairs) = copy_pairs.get(s..e) {
                for &(_, src) in pairs {
                    reg(src);
                }
            }
        }
    }
}

fn compute_vreg_use_counts<'a>(func: &MFunc<'_>, alloc: Alloc<'a>) -> Vec<u32, Alloc<'a>> {
    if !func.is_ssa || func.vreg_class.is_empty() {
        return Vec::new_in(alloc);
    }
    let mut counts: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(func.vreg_class.len(), alloc);
    for _ in 0..func.vreg_class.len() {
        counts.push(0);
    }
    for op in func.ops.iter() {
        visit_op_vreg_uses(op, &func.copy_pairs, |vi| {
            if let Some(slot) = counts.get_mut(vi) {
                *slot = slot.saturating_add(1);
            }
        });
    }
    counts
}

/// Visits every `VReg` written (defined) by `op` (for exact SSA single-def verification).
fn visit_op_vreg_defs(op: &MOp, copy_pairs: &[(MReg, MReg)], mut f: impl FnMut(usize)) {
    let mut reg = |r: MReg| {
        if let MReg::V(v) = r {
            f(to_idx(v.0));
        }
    };
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
        | MOp::AdrTable { dst, .. } => reg(dst),
        MOp::Ldr { dst, addr, .. } => {
            reg(dst);
            if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                reg(base);
            }
        }
        MOp::Str { addr, .. } => {
            if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                reg(base);
            }
        }
        MOp::Ldp { dst0, dst1, base, post_idx, pre_idx, .. } => {
            reg(dst0);
            reg(dst1);
            if post_idx || pre_idx {
                reg(base);
            }
        }
        MOp::Stp { base, post_idx, pre_idx, .. } => {
            if post_idx || pre_idx {
                reg(base);
            }
        }
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(copy_pairs.len());
            if let Some(pairs) = copy_pairs.get(s..e) {
                for &(dst, _) in pairs {
                    reg(dst);
                }
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

fn compute_vreg_def_counts<'a>(func: &MFunc<'_>, alloc: Alloc<'a>) -> Vec<u32, Alloc<'a>> {
    if !func.is_ssa || func.vreg_class.is_empty() {
        return Vec::new_in(alloc);
    }
    let mut counts: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(func.vreg_class.len(), alloc);
    for _ in 0..func.vreg_class.len() {
        counts.push(0);
    }
    for op in func.ops.iter() {
        visit_op_vreg_defs(op, &func.copy_pairs, |vi| {
            if let Some(slot) = counts.get_mut(vi) {
                *slot = slot.saturating_add(1);
            }
        });
    }
    counts
}

#[inline]
fn resolve_vreg_remap(remap: &[u32], v: VReg) -> VReg {
    let mut cur = v.0;
    for _ in 0..MAX_THREAD_HOPS {
        let next = remap.get(to_idx(cur)).copied().unwrap_or(u32::MAX);
        if next == u32::MAX || next == cur {
            break;
        }
        cur = next;
    }
    VReg(cur)
}

/// Rewrites every pure read operand `MReg::V(v)` inside `op` (and `copy_pairs` sources for
/// `ParallelCopy`) according to `remap` (`u32::MAX` = unchanged).
fn remap_op_vreg_uses(op: &mut MOp, copy_pairs: &mut [(MReg, MReg)], remap: &[u32]) {
    let map_slot = |slot: &mut MReg| {
        if let MReg::V(v) = *slot {
            let resolved = resolve_vreg_remap(remap, v);
            if resolved != v {
                *slot = MReg::V(resolved);
            }
        }
    };
    match op {
        MOp::Mov { src, .. } | MOp::MovSp { src, .. } => map_slot(src),
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
        MOp::Blr { target } | MOp::Br { target } => map_slot(target),
        MOp::AddImm { rn, .. }
        | MOp::SubImm { rn, .. }
        | MOp::LogicalImm { rn, .. }
        | MOp::Ubfm { rn, .. }
        | MOp::Sbfm { rn, .. }
        | MOp::Bfxil { rn, .. }
        | MOp::Rbit { rn, .. }
        | MOp::Clz { rn, .. }
        | MOp::Rev { rn, .. }
        | MOp::CmpImm { rn, .. }
        | MOp::TstImm { rn, .. }
        | MOp::CcmpImm { rn, .. }
        | MOp::AddLo12 { rn, .. } => map_slot(rn),
        MOp::Mvn { rm, .. } => map_slot(rm),
        MOp::AddReg { rn, rm, .. }
        | MOp::SubReg { rn, rm, .. }
        | MOp::AddExt { rn, rm, .. }
        | MOp::SubExt { rn, rm, .. }
        | MOp::LogicalReg { rn, rm, .. }
        | MOp::ShiftVar { rn, rm, .. }
        | MOp::Extr { rn, rm, .. }
        | MOp::Mul { rn, rm, .. }
        | MOp::Umulh { rn, rm, .. }
        | MOp::Smulh { rn, rm, .. }
        | MOp::CarryAddSub { rn, rm, .. }
        | MOp::Udiv { rn, rm, .. }
        | MOp::Sdiv { rn, rm, .. }
        | MOp::Umin { rn, rm, .. }
        | MOp::Umax { rn, rm, .. }
        | MOp::Smin { rn, rm, .. }
        | MOp::Smax { rn, rm, .. }
        | MOp::CmpReg { rn, rm, .. }
        | MOp::TstReg { rn, rm, .. }
        | MOp::CcmpReg { rn, rm, .. }
        | MOp::Csel { rn, rm, .. }
        | MOp::Csinc { rn, rm, .. }
        | MOp::Csinv { rn, rm, .. }
        | MOp::Csneg { rn, rm, .. }
        | MOp::FpCmp { rn, rm, .. }
        | MOp::Fcsel { rn, rm, .. } => {
            map_slot(rn);
            map_slot(rm);
        }
        MOp::Madd { rn, rm, ra, .. } | MOp::Msub { rn, rm, ra, .. } => {
            map_slot(rn);
            map_slot(rm);
            map_slot(ra);
        }
        MOp::FpAlu { rn, rm, ra, .. } => {
            map_slot(rn);
            if let Some(m) = rm {
                map_slot(m);
            }
            if let Some(a) = ra {
                map_slot(a);
            }
        }
        MOp::VecAlu { op: vop, rn, rm, .. } => {
            if !matches!(*vop, VecAluOp::Zero) {
                map_slot(rn);
                if let Some(m) = rm {
                    map_slot(m);
                }
            }
        }
        MOp::Ldr { addr, .. } => match addr {
            AddrMode::BaseOff { base, .. } => map_slot(base),
            AddrMode::ScaledIdx { base, idx, .. } => {
                map_slot(base);
                map_slot(idx);
            }
            AddrMode::PostIdx { .. } | AddrMode::PreIdx { .. } => {}
        },
        MOp::Str { src, addr, .. } => {
            map_slot(src);
            match addr {
                AddrMode::BaseOff { base, .. } => map_slot(base),
                AddrMode::ScaledIdx { base, idx, .. } => {
                    map_slot(base);
                    map_slot(idx);
                }
                AddrMode::PostIdx { .. } | AddrMode::PreIdx { .. } => {}
            }
        }
        MOp::Ldp { base, post_idx, pre_idx, .. } => {
            if !*post_idx && !*pre_idx {
                map_slot(base);
            }
        }
        MOp::Prfm { base, .. } => map_slot(base),
        MOp::Stp { src0, src1, base, post_idx, pre_idx, .. } => {
            map_slot(src0);
            map_slot(src1);
            if !*post_idx && !*pre_idx {
                map_slot(base);
            }
        }
        MOp::Cbz { rt, .. }
        | MOp::Cbnz { rt, .. }
        | MOp::Tbz { rt, .. }
        | MOp::Tbnz { rt, .. } => map_slot(rt),
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(*start);
            let e = s.saturating_add(usize::from(*len)).min(copy_pairs.len());
            if let Some(pairs) = copy_pairs.get_mut(s..e) {
                for (_, src) in pairs {
                    map_slot(src);
                }
            }
        }
    }
}

/// Returns `true` iff every read of `reg` inside `op` strictly reads at most the low 32 bits
/// (`bits 0..31`) of `reg` (never a memory address base/index, never in `ParallelCopy`, never a
/// read-modify-write operand, and never a 64-bit operand). Returns `true` if `op` does not read
/// `reg`.
fn op_uses_reg_only_low32(op: &MOp, copy_pairs: &[(MReg, MReg)], reg: MReg) -> bool {
    match *op {
        MOp::Mov { src, wide, .. } => src != reg || !wide,
        MOp::MovSp { src, .. } => src != reg,
        MOp::MovImm { .. }
        | MOp::Adrp { .. }
        | MOp::AdrTable { .. }
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop => true,
        MOp::AddImm { rn, wide, .. }
        | MOp::SubImm { rn, wide, .. }
        | MOp::LogicalImm { rn, wide, .. }
        | MOp::Ubfm { rn, wide, .. }
        | MOp::Sbfm { rn, wide, .. }
        | MOp::Rbit { rn, wide, .. }
        | MOp::Clz { rn, wide, .. }
        | MOp::Rev { rn, wide, .. }
        | MOp::CmpImm { rn, wide, .. }
        | MOp::TstImm { rn, wide, .. }
        | MOp::CcmpImm { rn, wide, .. } => rn != reg || !wide,
        MOp::Mvn { rm, wide, .. } => rm != reg || !wide,
        MOp::Bfxil { dst, rn, wide, .. } => dst != reg && (rn != reg || !wide),
        MOp::AddExt { rn, wide, .. } | MOp::SubExt { rn, wide, .. } => {
            // All `MExt` variants (`Uxtb`/`Uxth`/`Uxtw`/`Sxtw`) read at most bits `0..31` of `rm`.
            rn != reg || !wide
        }
        MOp::AddReg { rn, rm, wide, .. }
        | MOp::SubReg { rn, rm, wide, .. }
        | MOp::LogicalReg { rn, rm, wide, .. }
        | MOp::ShiftVar { rn, rm, wide, .. }
        | MOp::Extr { rn, rm, wide, .. }
        | MOp::Mul { rn, rm, wide, .. }
        | MOp::Udiv { rn, rm, wide, .. }
        | MOp::Sdiv { rn, rm, wide, .. }
        | MOp::Umin { rn, rm, wide, .. }
        | MOp::Umax { rn, rm, wide, .. }
        | MOp::Smin { rn, rm, wide, .. }
        | MOp::Smax { rn, rm, wide, .. }
        | MOp::CmpReg { rn, rm, wide, .. }
        | MOp::TstReg { rn, rm, wide, .. }
        | MOp::CcmpReg { rn, rm, wide, .. }
        | MOp::Csel { rn, rm, wide, .. }
        | MOp::Csinc { rn, rm, wide, .. }
        | MOp::Csinv { rn, rm, wide, .. }
        | MOp::Csneg { rn, rm, wide, .. } => !wide || (rn != reg && rm != reg),
        MOp::Madd { rn, rm, ra, wide, .. } | MOp::Msub { rn, rm, ra, wide, .. } => {
            !wide || (rn != reg && rm != reg && ra != reg)
        }
        MOp::Umulh { rn, rm, .. }
        | MOp::Smulh { rn, rm, .. }
        | MOp::CarryAddSub { rn, rm, .. } => rn != reg && rm != reg,
        MOp::AddLo12 { rn, .. } => rn != reg,
        MOp::FpAlu { op: fp_op, rn, double, .. } => match fp_op {
            FpAluOp::MovFromGpr => !double || rn != reg,
            FpAluOp::Scvtf { int_wide } | FpAluOp::Ucvtf { int_wide } => !int_wide || rn != reg,
            _ => rn != reg,
        },
        MOp::FpCmp { .. } | MOp::Fcsel { .. } => true,
        MOp::VecAlu { op: VecAluOp::DupGen, rn, elem_size, .. } => elem_size <= 2 || rn != reg,
        MOp::VecAlu { .. } => true,
        MOp::Ldr { addr, .. } => !addr_uses_reg(addr, reg),
        MOp::Str { src, addr, size } => {
            if addr_uses_reg(addr, reg) {
                return false;
            }
            if src != reg {
                return true;
            }
            let has_wb = matches!(addr, AddrMode::PostIdx { .. } | AddrMode::PreIdx { .. });
            !has_wb && matches!(size, MemSize::B8 | MemSize::H16 | MemSize::W32)
        }
        MOp::Ldp { base, .. } | MOp::Prfm { base, .. } => base != reg,
        MOp::Stp { src0, src1, base, size, post_idx, pre_idx, .. } => {
            if base == reg {
                return false;
            }
            if src0 != reg && src1 != reg {
                return true;
            }
            !(post_idx || pre_idx) && size == MemSize::W32
        }
        MOp::Cbz { rt, wide, .. } | MOp::Cbnz { rt, wide, .. } => rt != reg || !wide,
        MOp::Tbz { rt, bit, .. } | MOp::Tbnz { rt, bit, .. } => rt != reg || bit < 32,
        MOp::Blr { target } | MOp::Br { target } => reg.is_virtual() && target != reg,
        MOp::Bl | MOp::Ret => reg.is_virtual(),
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(copy_pairs.len());
            if let Some(pairs) = copy_pairs.get(s..e) {
                for &(_, src) in pairs {
                    if src == reg {
                        return false;
                    }
                }
            }
            true
        }
        MOp::RawWord { uses_gpr, uses_fp, .. } => !raw_mask_hits_reg(uses_gpr, uses_fp, reg),
    }
}

#[inline]
fn dec_op_vreg_uses(op: &MOp, copy_pairs: &[(MReg, MReg)], counts: &mut [u32]) {
    if counts.is_empty() {
        return;
    }
    visit_op_vreg_uses(op, copy_pairs, |vi| {
        if let Some(slot) = counts.get_mut(vi) {
            *slot = slot.saturating_sub(1);
        }
    });
}

#[inline]
fn inc_op_vreg_uses(op: &MOp, copy_pairs: &[(MReg, MReg)], counts: &mut [u32]) {
    if counts.is_empty() {
        return;
    }
    visit_op_vreg_uses(op, copy_pairs, |vi| {
        if let Some(slot) = counts.get_mut(vi) {
            *slot = slot.saturating_add(1);
        }
    });
}

/// True iff `reg` is provably dead immediately after `after_idx` (checked up to `block_end`, or
/// globally across the function via `vreg_uses` when `func.is_ssa == true`).
fn is_reg_dead_after(
    func: &MFunc<'_>,
    block_end: usize,
    after_idx: usize,
    reg: MReg,
    vreg_uses: &[u32],
) -> bool {
    if !is_trackable_reg(reg) {
        return true;
    }
    if func.is_ssa
        && let MReg::V(v) = reg
    {
        return vreg_uses.get(to_idx(v.0)).copied().unwrap_or(1) == 0;
    }
    for k in (after_idx + 1)..block_end {
        let op_k = op_at(func, k);
        if matches!(op_k, MOp::Nop) {
            continue;
        }
        if inst_uses_reg(func, k, reg) {
            return false;
        }
        if inst_defines_reg(func, k, reg) {
            return true;
        }
        if matches!(op_k, MOp::Ret) {
            if !func.is_ssa
                && let MReg::P(PReg::Gpr(r)) = reg
                && r < 30
                && (func.ret_live_gprs & (1u32 << r)) == 0
            {
                return true;
            }
            return false;
        }
        if is_control_or_call(&op_k) {
            return false;
        }
    }
    false
}

/// Result of attempting to rewrite pure register uses of `old_reg` -> `new_reg` inside one `MOp`.
struct RewriteUsesOut {
    rewrote_any: bool,
    had_unrewritable_use: bool,
}

/// Rewrites pure GPR/SP uses of `old_reg` to `new_reg` inside `op`, strictly respecting
/// `OperandRole::GprOrZr` (`Sp` forbidden) vs `OperandRole::GprOrSp` (`Xzr` forbidden) and never
/// rewriting read-modify-write destination operands (`Bfxil`, writeback `base`, etc.).
fn rewrite_gpr_uses_in_op(op: &mut MOp, old_reg: MReg, new_reg: MReg) -> RewriteUsesOut {
    let mut out = RewriteUsesOut { rewrote_any: false, had_unrewritable_use: false };
    let allow_zr = new_reg != MReg::SP;
    let allow_sp = new_reg != MReg::XZR;

    let try_slot = |out: &mut RewriteUsesOut, slot: &mut MReg, allow: bool| {
        if *slot == old_reg {
            if allow {
                *slot = new_reg;
                out.rewrote_any = true;
            } else {
                out.had_unrewritable_use = true;
            }
        }
    };

    match op {
        MOp::Mov { src, .. } => try_slot(&mut out, src, allow_zr),
        MOp::MovSp { src, .. } => try_slot(&mut out, src, allow_sp),
        MOp::AddImm { rn, .. }
        | MOp::SubImm { rn, .. }
        | MOp::CmpImm { rn, .. }
        | MOp::AddLo12 { rn, .. } => try_slot(&mut out, rn, allow_sp),
        MOp::LogicalImm { rn, .. }
        | MOp::Ubfm { rn, .. }
        | MOp::Sbfm { rn, .. }
        | MOp::Rbit { rn, .. }
        | MOp::Clz { rn, .. }
        | MOp::Rev { rn, .. }
        | MOp::TstImm { rn, .. }
        | MOp::CcmpImm { rn, .. } => try_slot(&mut out, rn, allow_zr),
        MOp::Mvn { rm, .. } => try_slot(&mut out, rm, allow_zr),
        MOp::Bfxil { dst, rn, .. } => {
            if *dst == old_reg {
                out.had_unrewritable_use = true;
            }
            try_slot(&mut out, rn, allow_zr);
        }
        MOp::AddExt { rn, rm, .. } | MOp::SubExt { rn, rm, .. } => {
            try_slot(&mut out, rn, allow_sp);
            try_slot(&mut out, rm, allow_zr);
        }
        MOp::AddReg { rn, rm, .. }
        | MOp::SubReg { rn, rm, .. }
        | MOp::LogicalReg { rn, rm, .. }
        | MOp::ShiftVar { rn, rm, .. }
        | MOp::Extr { rn, rm, .. }
        | MOp::Mul { rn, rm, .. }
        | MOp::Umulh { rn, rm, .. }
        | MOp::Smulh { rn, rm, .. }
        | MOp::CarryAddSub { rn, rm, .. }
        | MOp::Udiv { rn, rm, .. }
        | MOp::Sdiv { rn, rm, .. }
        | MOp::Umin { rn, rm, .. }
        | MOp::Umax { rn, rm, .. }
        | MOp::Smin { rn, rm, .. }
        | MOp::Smax { rn, rm, .. }
        | MOp::CmpReg { rn, rm, .. }
        | MOp::TstReg { rn, rm, .. }
        | MOp::CcmpReg { rn, rm, .. }
        | MOp::Csel { rn, rm, .. }
        | MOp::Csinc { rn, rm, .. }
        | MOp::Csinv { rn, rm, .. }
        | MOp::Csneg { rn, rm, .. } => {
            try_slot(&mut out, rn, allow_zr);
            try_slot(&mut out, rm, allow_zr);
        }
        MOp::Madd { rn, rm, ra, .. } | MOp::Msub { rn, rm, ra, .. } => {
            try_slot(&mut out, rn, allow_zr);
            try_slot(&mut out, rm, allow_zr);
            try_slot(&mut out, ra, allow_zr);
        }
        MOp::FpAlu { op: fp_op, rn, .. } => {
            if matches!(
                *fp_op,
                FpAluOp::MovFromGpr | FpAluOp::Scvtf { .. } | FpAluOp::Ucvtf { .. }
            ) {
                try_slot(&mut out, rn, allow_zr);
            }
        }
        MOp::VecAlu { op: VecAluOp::DupGen, rn, .. } => try_slot(&mut out, rn, allow_zr),
        MOp::Ldr { addr, .. } => match addr {
            AddrMode::BaseOff { base, .. } => try_slot(&mut out, base, allow_sp),
            AddrMode::ScaledIdx { base, idx, .. } => {
                try_slot(&mut out, base, allow_sp);
                try_slot(&mut out, idx, allow_zr);
            }
            AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => {
                if *base == old_reg {
                    out.had_unrewritable_use = true;
                }
            }
        },
        MOp::Str { src, addr, size } => {
            if !size.is_fp_or_vec() {
                // Reject `Str` with writeback where rewriting `src` to `base` would violate ISA.
                let wb_base = match *addr {
                    AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => Some(base),
                    AddrMode::BaseOff { .. } | AddrMode::ScaledIdx { .. } => None,
                };
                if *src == old_reg && wb_base == Some(new_reg) {
                    out.had_unrewritable_use = true;
                } else {
                    try_slot(&mut out, src, allow_zr);
                }
            }
            match addr {
                AddrMode::BaseOff { base, .. } => try_slot(&mut out, base, allow_sp),
                AddrMode::ScaledIdx { base, idx, .. } => {
                    try_slot(&mut out, base, allow_sp);
                    try_slot(&mut out, idx, allow_zr);
                }
                AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => {
                    if *base == old_reg {
                        out.had_unrewritable_use = true;
                    }
                }
            }
        }
        MOp::Ldp { base, post_idx, pre_idx, .. } => {
            if *post_idx || *pre_idx {
                if *base == old_reg {
                    out.had_unrewritable_use = true;
                }
            } else {
                try_slot(&mut out, base, allow_sp);
            }
        }
        MOp::Prfm { base, .. } => try_slot(&mut out, base, allow_sp),
        MOp::Stp { src0, src1, base, size, post_idx, pre_idx, .. } => {
            if !size.is_fp_or_vec() {
                let wb = *post_idx || *pre_idx;
                if *src0 == old_reg && wb && *base == new_reg {
                    out.had_unrewritable_use = true;
                } else {
                    try_slot(&mut out, src0, allow_zr);
                }
                if *src1 == old_reg && wb && *base == new_reg {
                    out.had_unrewritable_use = true;
                } else {
                    try_slot(&mut out, src1, allow_zr);
                }
            }
            if *post_idx || *pre_idx {
                if *base == old_reg {
                    out.had_unrewritable_use = true;
                }
            } else {
                try_slot(&mut out, base, allow_sp);
            }
        }
        MOp::Cbz { rt, .. }
        | MOp::Cbnz { rt, .. }
        | MOp::Tbz { rt, .. }
        | MOp::Tbnz { rt, .. }
        | MOp::Blr { target: rt }
        | MOp::Br { target: rt } => try_slot(&mut out, rt, allow_zr),
        MOp::MovImm { .. }
        | MOp::Adrp { .. }
        | MOp::AdrTable { .. }
        | MOp::FpCmp { .. }
        | MOp::Fcsel { .. }
        | MOp::VecAlu { .. }
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop => {}
        MOp::Bl | MOp::Ret | MOp::ParallelCopy { .. } | MOp::RawWord { .. } => {
            // Conservatively leave opaque / ABI-fixed / parallel-copy uses untouched.
            out.had_unrewritable_use = true;
        }
    }
    out
}

/// Rewrites pure 32-bit-only GPR uses of `old_reg` to `new_reg` inside `op` (for narrow
/// `Mov { dst: old_reg, src: new_reg, wide: false }` copy propagation). Any 64-bit, pointer,
/// read-modify-write, or opaque use of `old_reg` marks `had_unrewritable_use = true` so the
/// zero-extending `Mov` is preserved whenever upper 32 bits are observable.
fn rewrite_narrow32_uses_in_op(op: &mut MOp, old_reg: MReg, new_reg: MReg) -> RewriteUsesOut {
    let mut out = RewriteUsesOut { rewrote_any: false, had_unrewritable_use: false };
    let allow_zr = new_reg != MReg::SP;
    let allow_sp = new_reg != MReg::XZR;

    let try_slot = |out: &mut RewriteUsesOut, slot: &mut MReg, allow: bool| {
        if *slot == old_reg {
            if allow {
                *slot = new_reg;
                out.rewrote_any = true;
            } else {
                out.had_unrewritable_use = true;
            }
        }
    };

    match op {
        MOp::Mov { src, wide, .. } => {
            if !*wide {
                try_slot(&mut out, src, allow_zr);
            } else if *src == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::MovSp { src, .. } => {
            if *src == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::AddImm { rn, wide, .. }
        | MOp::SubImm { rn, wide, .. }
        | MOp::CmpImm { rn, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rn, allow_sp);
            } else if *rn == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::LogicalImm { rn, wide, .. }
        | MOp::Ubfm { rn, wide, .. }
        | MOp::Sbfm { rn, wide, .. }
        | MOp::Rbit { rn, wide, .. }
        | MOp::Clz { rn, wide, .. }
        | MOp::Rev { rn, wide, .. }
        | MOp::TstImm { rn, wide, .. }
        | MOp::CcmpImm { rn, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rn, allow_zr);
            } else if *rn == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Mvn { rm, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rm, allow_zr);
            } else if *rm == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Bfxil { dst, rn, wide, .. } => {
            if *dst == old_reg {
                out.had_unrewritable_use = true;
            }
            if !*wide {
                try_slot(&mut out, rn, allow_zr);
            } else if *rn == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::AddExt { rn, rm, wide, .. } | MOp::SubExt { rn, rm, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rn, allow_sp);
            } else if *rn == old_reg {
                out.had_unrewritable_use = true;
            }
            // All `MExt` variants (`Uxtb`/`Uxth`/`Uxtw`/`Sxtw`) read at most bits `0..31` of `rm`.
            try_slot(&mut out, rm, allow_zr);
        }
        MOp::AddReg { rn, rm, wide, .. }
        | MOp::SubReg { rn, rm, wide, .. }
        | MOp::LogicalReg { rn, rm, wide, .. }
        | MOp::ShiftVar { rn, rm, wide, .. }
        | MOp::Extr { rn, rm, wide, .. }
        | MOp::Mul { rn, rm, wide, .. }
        | MOp::Udiv { rn, rm, wide, .. }
        | MOp::Sdiv { rn, rm, wide, .. }
        | MOp::Umin { rn, rm, wide, .. }
        | MOp::Umax { rn, rm, wide, .. }
        | MOp::Smin { rn, rm, wide, .. }
        | MOp::Smax { rn, rm, wide, .. }
        | MOp::CmpReg { rn, rm, wide, .. }
        | MOp::TstReg { rn, rm, wide, .. }
        | MOp::CcmpReg { rn, rm, wide, .. }
        | MOp::Csel { rn, rm, wide, .. }
        | MOp::Csinc { rn, rm, wide, .. }
        | MOp::Csinv { rn, rm, wide, .. }
        | MOp::Csneg { rn, rm, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rn, allow_zr);
                try_slot(&mut out, rm, allow_zr);
            } else if *rn == old_reg || *rm == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Madd { rn, rm, ra, wide, .. } | MOp::Msub { rn, rm, ra, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rn, allow_zr);
                try_slot(&mut out, rm, allow_zr);
                try_slot(&mut out, ra, allow_zr);
            } else if *rn == old_reg || *rm == old_reg || *ra == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Umulh { rn, rm, .. }
        | MOp::Smulh { rn, rm, .. }
        | MOp::CarryAddSub { rn, rm, .. } => {
            if *rn == old_reg || *rm == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::FpAlu { op: fp_op, rn, double, .. } => match *fp_op {
            FpAluOp::MovFromGpr => {
                if !*double {
                    try_slot(&mut out, rn, allow_zr);
                } else if *rn == old_reg {
                    out.had_unrewritable_use = true;
                }
            }
            FpAluOp::Scvtf { int_wide } | FpAluOp::Ucvtf { int_wide } => {
                if !int_wide {
                    try_slot(&mut out, rn, allow_zr);
                } else if *rn == old_reg {
                    out.had_unrewritable_use = true;
                }
            }
            _ => {}
        },
        MOp::VecAlu { op: VecAluOp::DupGen, rn, elem_size, .. } => {
            if *elem_size <= 2 {
                try_slot(&mut out, rn, allow_zr);
            } else if *rn == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Str { src, addr, size } => {
            if matches!(*size, MemSize::B8 | MemSize::H16 | MemSize::W32) {
                let wb_base = match *addr {
                    AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => Some(base),
                    AddrMode::BaseOff { .. } | AddrMode::ScaledIdx { .. } => None,
                };
                if *src == old_reg && wb_base == Some(new_reg) {
                    out.had_unrewritable_use = true;
                } else {
                    try_slot(&mut out, src, allow_zr);
                }
            } else if *size == MemSize::X64 && *src == old_reg {
                out.had_unrewritable_use = true;
            }
            if addr_uses_reg(*addr, old_reg) {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Stp { src0, src1, base, size, post_idx, pre_idx, .. } => {
            if *size == MemSize::W32 {
                let wb = *post_idx || *pre_idx;
                if *src0 == old_reg && wb && *base == new_reg {
                    out.had_unrewritable_use = true;
                } else {
                    try_slot(&mut out, src0, allow_zr);
                }
                if *src1 == old_reg && wb && *base == new_reg {
                    out.had_unrewritable_use = true;
                } else {
                    try_slot(&mut out, src1, allow_zr);
                }
            } else if *size == MemSize::X64 && (*src0 == old_reg || *src1 == old_reg) {
                out.had_unrewritable_use = true;
            }
            if *base == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Ldr { addr, .. } => {
            if addr_uses_reg(*addr, old_reg) {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Ldp { base, .. } | MOp::Prfm { base, .. } | MOp::AddLo12 { rn: base, .. } => {
            if *base == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Cbz { rt, wide, .. } | MOp::Cbnz { rt, wide, .. } => {
            if !*wide {
                try_slot(&mut out, rt, allow_zr);
            } else if *rt == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Tbz { rt, bit, .. } | MOp::Tbnz { rt, bit, .. } => {
            if *bit < 32 {
                try_slot(&mut out, rt, allow_zr);
            } else if *rt == old_reg {
                out.had_unrewritable_use = true;
            }
        }
        MOp::Blr { target } | MOp::Br { target } => {
            if *target == old_reg || !old_reg.is_virtual() {
                out.had_unrewritable_use = true;
            }
        }
        MOp::MovImm { .. }
        | MOp::Adrp { .. }
        | MOp::AdrTable { .. }
        | MOp::FpCmp { .. }
        | MOp::Fcsel { .. }
        | MOp::VecAlu { .. }
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop => {}
        MOp::Bl | MOp::Ret | MOp::ParallelCopy { .. } | MOp::RawWord { .. } => {
            out.had_unrewritable_use = true;
        }
    }
    out
}

// -----------------------------------------------------------------------------
// Flag-consumer predicates (typed `MCond`, zero bitmask decoding)
// -----------------------------------------------------------------------------

#[inline]
const fn cond_only_nz(cond: MCond) -> bool {
    matches!(cond, MCond::Eq | MCond::Ne | MCond::Mi | MCond::Pl)
}

#[inline]
const fn cond_only_z(cond: MCond) -> bool {
    matches!(cond, MCond::Eq | MCond::Ne)
}

/// Checks whether every consumer of the NZCV flags written at `flag_writer_idx` in
/// `(flag_writer_idx + 1)..block_end` satisfies `cond_ok`, with at least one in-block consumer.
fn flags_consumers_satisfy(
    func: &MFunc<'_>,
    flag_writer_idx: usize,
    block_end: usize,
    cond_ok: fn(MCond) -> bool,
) -> bool {
    let mut saw_consumer = false;
    for k in (flag_writer_idx + 1)..block_end {
        let op = op_at(func, k);
        if matches!(op, MOp::Nop) {
            continue;
        }
        if op.reads_flags() {
            let ok = match op {
                MOp::BCond { cond, .. }
                | MOp::Csel { cond, .. }
                | MOp::Csinc { cond, .. }
                | MOp::Csinv { cond, .. }
                | MOp::Csneg { cond, .. }
                | MOp::Fcsel { cond, .. } => cond_ok(cond),
                MOp::CcmpImm { .. } | MOp::CcmpReg { .. } | MOp::RawWord { .. } => false,
                _ => false,
            };
            if !ok {
                return false;
            }
            saw_consumer = true;
        }
        if op.writes_flags() {
            // Flags overwritten before escaping the block: all consumers verified above.
            return saw_consumer;
        }
        if matches!(op, MOp::Br { .. } | MOp::Brk { .. } | MOp::Udf | MOp::RawWord { .. }) {
            return false;
        }
        if matches!(op, MOp::Ret) {
            // If no consumer existed before `Ret`, flags might be a `BlazeResult` carry return.
            return saw_consumer;
        }
    }
    saw_consumer
}

// -----------------------------------------------------------------------------
// Pass 0 (SSA Only): Global Multi-Instruction `MovImm` CSE & Dead Narrow-32 `Mov` Elision
// -----------------------------------------------------------------------------

/// Computes the exact AArch64 instruction word count (`1..=4`) emitted by `mencode` for
/// `MOp::MovImm { imm, wide }` (matching `super::mfunc::encode_mov_imm_words`).
#[inline]
fn mov_imm_inst_cost(imm: u64, wide: bool) -> u32 {
    if !wide {
        let val = imm & 0xffff_ffff;
        let lo = chunk16(val, 0);
        let hi = chunk16(val, 1);
        if hi == 0 || lo == 0 || hi == 0xffff || lo == 0xffff {
            return 1;
        }
        if enc::logical_imm(val, 32).is_some() {
            return 1;
        }
        return 2;
    }

    let chunks = [
        chunk16(imm, 0),
        chunk16(imm, 1),
        chunk16(imm, 2),
        chunk16(imm, 3),
    ];
    let mut zero_cnt: u32 = 0;
    let mut ones_cnt: u32 = 0;
    for &c in &chunks {
        if c == 0 {
            zero_cnt += 1;
        }
        if c == 0xffff {
            ones_cnt += 1;
        }
    }
    if zero_cnt >= 3 || ones_cnt >= 3 {
        return 1;
    }
    if enc::logical_imm(imm, 64).is_some() {
        return 1;
    }
    if ones_cnt > zero_cnt {
        4 - ones_cnt
    } else {
        4 - zero_cnt
    }
}

#[derive(Clone, Copy)]
struct GlobalImmEntry {
    valid: bool,
    imm: u64,
    wide: bool,
    cost: u32,
    count: u32,
    in_loop: bool,
}

impl GlobalImmEntry {
    const EMPTY: Self = Self {
        valid: false,
        imm: 0,
        wide: false,
        cost: 0,
        count: 0,
        in_loop: false,
    };
}

/// Pass 0a (SSA only): hoists frequent (`count >= 2`) or in-loop (`cost >= 3`) multi-instruction
/// `MovImm` constants (`cost >= 2`, up to [`MAX_HOISTED_IMMS`]) into `entry_blk = block_order[0]`,
/// replacing all duplicate single-def `VReg`s across the entire function (including `copy_pairs`).
fn cse_global_multi_inst_mov_imm<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
    stats: &mut MOptStats,
) {
    if !func.is_ssa || func.block_order.is_empty() || func.vreg_class.is_empty() {
        return;
    }
    let entry_blk = func.block_order[0];
    let entry_idx = to_idx(entry_blk.0);
    if entry_idx >= func.block_insts.len() || !func.block_preds(entry_blk).is_empty() {
        return;
    }

    let num_blocks = func.block_insts.len();
    let n_order = func.block_order.len();

    // Build `block_pos[blk]` and detect loop blocks + any back-edge into `entry_blk`.
    let mut block_pos: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    let mut in_loop: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for b in 0..num_blocks {
        block_pos.push(u32::MAX);
        in_loop.push(func.block_align16.get(b).copied().unwrap_or(false));
    }
    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        let bi = to_idx(blk.0);
        if bi < num_blocks {
            set(&mut block_pos, bi, u32_of(ord_pos).unwrap_or(u32::MAX));
        }
    }
    for ord_pos in 0..n_order {
        let Some(&blk) = func.block_order.get(ord_pos) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        for i in start..end {
            if let Some(target) = op_at(func, i).branch_target() {
                if target == entry_blk {
                    // Entry block has a predecessor / back-edge: skip hoisting.
                    return;
                }
                let ti = to_idx(target.0);
                if ti < num_blocks {
                    let t_pos = at(&block_pos, ti, u32::MAX);
                    if t_pos != u32::MAX && to_idx(t_pos) <= ord_pos {
                        for k in to_idx(t_pos)..=ord_pos {
                            if let Some(&loop_blk) = func.block_order.get(k) {
                                set(&mut in_loop, to_idx(loop_blk.0), true);
                            }
                        }
                    }
                }
            }
        }
    }

    let def_counts = compute_vreg_def_counts(func, alloc);
    let use_counts = compute_vreg_use_counts(func, alloc);

    // Count multi-instruction `MovImm` candidates in deterministic encounter order (no HashMap).
    let mut table = [GlobalImmEntry::EMPTY; GLOBAL_IMM_TABLE_CAP];
    for ord_pos in 0..n_order {
        let Some(&blk) = func.block_order.get(ord_pos) else {
            continue;
        };
        let bi = to_idx(blk.0);
        let blk_in_loop = at(&in_loop, bi, false);
        let (start, end) = block_range(func, blk);
        for i in start..end {
            if is_protected(func, i) {
                continue;
            }
            let MOp::MovImm { dst: MReg::V(v), imm, wide } = op_at(func, i) else {
                continue;
            };
            let vi = to_idx(v.0);
            if def_counts.get(vi).copied().unwrap_or(0) != 1
                || use_counts.get(vi).copied().unwrap_or(0) == 0
                || !func.vreg_class.get(vi).copied().is_some_and(VRegClass::is_gpr)
            {
                continue;
            }
            let imm_norm = if wide { imm } else { imm & 0xffff_ffff };
            let cost = mov_imm_inst_cost(imm_norm, wide);
            if cost < 2 {
                continue;
            }
            let mut found = false;
            for slot in &mut table {
                if slot.valid && slot.imm == imm_norm && slot.wide == wide {
                    slot.count = slot.count.saturating_add(1);
                    if blk_in_loop {
                        slot.in_loop = true;
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                for slot in &mut table {
                    if !slot.valid {
                        *slot = GlobalImmEntry {
                            valid: true,
                            imm: imm_norm,
                            wide,
                            cost,
                            count: 1,
                            in_loop: blk_in_loop,
                        };
                        break;
                    }
                }
            }
        }
    }

    // Select up to `MAX_HOISTED_IMMS` entries with highest `count * cost`, requiring
    // `count >= 2 || (in_loop && cost >= 3)`. Ties break deterministically by encounter order.
    let mut picked = [false; GLOBAL_IMM_TABLE_CAP];
    let mut selected_slots = [0usize; MAX_HOISTED_IMMS];
    let mut n_selected: usize = 0;
    for _ in 0..MAX_HOISTED_IMMS {
        let mut best_idx: Option<usize> = None;
        let mut best_score: u32 = 0;
        for (idx, slot) in table.iter().enumerate() {
            if !slot.valid || at(&picked, idx, true) {
                continue;
            }
            let eligible = slot.count >= 2 || (slot.in_loop && slot.cost >= 3);
            if !eligible {
                continue;
            }
            let score = slot.count.saturating_mul(slot.cost);
            if best_idx.is_none() || score > best_score {
                best_idx = Some(idx);
                best_score = score;
            }
        }
        let Some(chosen) = best_idx else {
            break;
        };
        set(&mut picked, chosen, true);
        set(&mut selected_slots, n_selected, chosen);
        n_selected += 1;
    }
    if n_selected == 0 || u32_of(func.ops.len().saturating_add(n_selected)).is_none() {
        return;
    }

    // Find a flag-free, terminator-free insertion point in `entry_blk` right after any leading
    // entry parameter captures (`ParallelCopy` from `PReg`s, ABI `PReg` `Mov`s, pinned `[SP, #off]`
    // stack-arg loads). None of these write flags or read any `VReg`.
    let Some(&(e_start_u, e_end_u)) = func.block_insts.get(entry_idx) else {
        return;
    };
    let e_start = to_idx(e_start_u);
    let e_end = to_idx(e_end_u).min(func.ops.len());
    let mut insert_pos = e_start;
    while insert_pos < e_end {
        let op = op_at(func, insert_pos);
        if op.writes_flags() || op.is_conditional_branch() || op.is_unconditional_terminator() {
            break;
        }
        let is_entry_capture = match op {
            MOp::Nop => true,
            MOp::ParallelCopy { start, len } => {
                let s = to_idx(start);
                let e = s.saturating_add(usize::from(len)).min(func.copy_pairs.len());
                let mut all_phys_src = true;
                if let Some(pairs) = func.copy_pairs.get(s..e) {
                    for &(_, src) in pairs {
                        if src.is_virtual() {
                            all_phys_src = false;
                            break;
                        }
                    }
                }
                all_phys_src
            }
            MOp::Mov { src: MReg::P(_), .. }
            | MOp::MovSp { src: MReg::P(_), .. }
            | MOp::FpAlu { op: FpAluOp::Mov, rn: MReg::P(_), .. }
            | MOp::VecAlu { op: VecAluOp::Mov, rn: MReg::P(_), .. } => true,
            MOp::Ldr { addr: AddrMode::BaseOff { base: MReg::P(PReg::Sp), .. }, .. } => {
                flags_at(func, insert_pos).contains(MInstFlags::PINNED)
            }
            _ => false,
        };
        if !is_entry_capture {
            break;
        }
        insert_pos += 1;
    }

    // Allocate one fresh `VReg` per hoisted `(imm, wide)` constant.
    let orig_vreg_len = func.vreg_class.len();
    let mut selected_info = [(0u64, false, VReg(0)); MAX_HOISTED_IMMS];
    for s_idx in 0..n_selected {
        let slot_idx = at(&selected_slots, s_idx, 0);
        let slot = at(&table, slot_idx, GlobalImmEntry::EMPTY);
        let cls = if slot.wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };
        let hoisted_v = func.alloc_vreg(cls);
        set(&mut selected_info, s_idx, (slot.imm, slot.wide, hoisted_v));
    }

    // Record `old_v -> hoisted_v` remap and delete all matching original `MovImm` instructions.
    let mut remap: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(func.vreg_class.len(), alloc);
    for _ in 0..func.vreg_class.len() {
        remap.push(u32::MAX);
    }
    for ord_pos in 0..n_order {
        let Some(&blk) = func.block_order.get(ord_pos) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        for i in start..end {
            if is_protected(func, i) {
                continue;
            }
            let MOp::MovImm { dst: MReg::V(v), imm, wide } = op_at(func, i) else {
                continue;
            };
            let vi = to_idx(v.0);
            if vi >= orig_vreg_len
                || def_counts.get(vi).copied().unwrap_or(0) != 1
                || use_counts.get(vi).copied().unwrap_or(0) == 0
                || !func.vreg_class.get(vi).copied().is_some_and(VRegClass::is_gpr)
            {
                continue;
            }
            let imm_norm = if wide { imm } else { imm & 0xffff_ffff };
            for &(s_imm, s_wide, hoisted_v) in selected_info.iter().take(n_selected) {
                if s_imm == imm_norm && s_wide == wide {
                    set(&mut remap, vi, hoisted_v.0);
                    mark_deleted(func, i);
                    stats.peepholes = stats.peepholes.saturating_add(1);
                    break;
                }
            }
        }
    }

    // Insert the `n_selected` hoisted `MovImm` instructions into `entry_blk` at `insert_pos`
    // across all five lockstep SoA columns and update every block range in `func.block_insts`.
    let old_len = func.ops.len();
    for _ in 0..n_selected {
        func.ops.push(MOp::Nop);
        func.flags.push(MInstFlags::NONE);
        func.vflag_def.push(NO_VFLAG);
        func.vflag_use.push(NO_VFLAG);
        func.note_idx.push(NO_NOTE);
    }
    for idx in (insert_pos..old_len).rev() {
        copy_inst_slot(func, idx + n_selected, idx);
    }
    for (offset, &(s_imm, s_wide, hoisted_v)) in selected_info.iter().take(n_selected).enumerate() {
        let pos = insert_pos + offset;
        set(&mut func.ops, pos, MOp::MovImm { dst: MReg::V(hoisted_v), imm: s_imm, wide: s_wide });
        set(&mut func.flags, pos, MInstFlags::REMATERIALIZABLE);
        set(&mut func.vflag_def, pos, NO_VFLAG);
        set(&mut func.vflag_use, pos, NO_VFLAG);
        set(&mut func.note_idx, pos, NO_NOTE);
    }

    let k_u32 = u32_of(n_selected).unwrap_or(0);
    for (bi, (s, e)) in func.block_insts.iter_mut().enumerate() {
        if bi == entry_idx {
            *e = e.saturating_add(k_u32);
        } else if *s >= e_end_u && !(*s == e_start_u && *e == e_start_u) {
            *s = s.saturating_add(k_u32);
            *e = e.saturating_add(k_u32);
        }
    }

    // Rewrite every use of the replaced duplicate `VReg`s across the entire function.
    for op in func.ops.iter_mut() {
        remap_op_vreg_uses(op, &mut func.copy_pairs, &remap);
    }
}

/// Pass 0b (SSA only): globally elides unpinned, noteless `Mov { dst: V(d), src: V(s), wide: false }`
/// whenever both `d` and `s` are single-def and EVERY use of `V(d)` across the entire function
/// strictly reads at most the low 32 bits (never in `ParallelCopy`, never as a memory address
/// base/index, never 64-bit, never in a protected instruction).
fn mopt_dead_narrow32_mov_ssa<'a>(func: &mut MFunc<'a>, alloc: Alloc<'a>, stats: &mut MOptStats) {
    if !func.is_ssa || func.vreg_class.is_empty() {
        return;
    }
    let n_vregs = func.vreg_class.len();
    let def_counts = compute_vreg_def_counts(func, alloc);

    // 1. Single O(N) sweep: determine which `VReg`s are ONLY ever read in strictly 32-bit,
    //    non-protected operand positions across the entire function.
    let mut only_low32_ok: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        only_low32_ok.push(true);
    }
    for j in 0..func.ops.len() {
        let op = op_at(func, j);
        if matches!(op, MOp::Nop) {
            continue;
        }
        let protected = is_protected(func, j);
        visit_op_vreg_uses(&op, &func.copy_pairs, |vi| {
            if let Some(slot) = only_low32_ok.get_mut(vi)
                && *slot
            {
                let vreg = MReg::V(VReg(u32_of(vi).unwrap_or(u32::MAX)));
                if protected || !op_uses_reg_only_low32(&op, &func.copy_pairs, vreg) {
                    *slot = false;
                }
            }
        });
    }

    // 2. Elide eligible `Mov { dst: V(d), src: V(s), wide: false }` and record `d -> s` remap.
    let mut remap: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        remap.push(u32::MAX);
    }
    let mut elided_any = false;
    for i in 0..func.ops.len() {
        if is_protected(func, i) {
            continue;
        }
        let MOp::Mov { dst: MReg::V(d), src: MReg::V(s), wide: false } = op_at(func, i) else {
            continue;
        };
        if d == s {
            continue;
        }
        let di = to_idx(d.0);
        let si = to_idx(s.0);
        if def_counts.get(di).copied().unwrap_or(0) != 1
            || def_counts.get(si).copied().unwrap_or(0) != 1
            || !func.vreg_class.get(di).copied().is_some_and(VRegClass::is_gpr)
            || !func.vreg_class.get(si).copied().is_some_and(VRegClass::is_gpr)
            || !only_low32_ok.get(di).copied().unwrap_or(false)
        {
            continue;
        }
        let mut cur = s.0;
        let mut hops: usize = 0;
        let mut safe_chain = true;
        while hops < MAX_THREAD_HOPS {
            if cur == d.0 {
                safe_chain = false;
                break;
            }
            let next = remap.get(to_idx(cur)).copied().unwrap_or(u32::MAX);
            if next == u32::MAX {
                break;
            }
            cur = next;
            hops += 1;
        }
        if !safe_chain || hops >= MAX_THREAD_HOPS {
            continue;
        }
        set(&mut remap, di, s.0);
        mark_deleted(func, i);
        stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
        elided_any = true;
    }

    // 3. Rewrite all uses across the function in one O(N) sweep.
    if elided_any {
        for op in func.ops.iter_mut() {
            remap_op_vreg_uses(op, &mut func.copy_pairs, &remap);
        }
    }
}

/// Pass 1m (SSA only): Whole-function pure wide-`Mov` (`Mov { dst: V(d), src: V(s), wide: true }`)
/// copy elimination across basic blocks when both `V(d)` and `V(s)` are single-def, non-phi
/// (`!is_phi`) GPR `VReg`s. Eliminates cross-block copies left behind after
/// `peephole_known_zero_upper32` upgrades zero-upper-32 `u32 -> u64` widens to `wide: true`.
fn mopt_global_wide_mov_ssa<'a>(func: &mut MFunc<'a>, alloc: Alloc<'a>, stats: &mut MOptStats) {
    if !func.is_ssa || func.vreg_class.is_empty() {
        return;
    }
    let n_vregs = func.vreg_class.len();
    let def_counts = compute_vreg_def_counts(func, alloc);
    let mut is_phi: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        is_phi.push(false);
    }
    for op in func.ops.iter() {
        if let MOp::ParallelCopy { start, len } = *op {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(func.copy_pairs.len());
            if let Some(pairs) = func.copy_pairs.get(s..e) {
                for &(dst, _) in pairs {
                    if let MReg::V(v) = dst {
                        set(&mut is_phi, to_idx(v.0), true);
                    }
                }
            }
        }
    }

    let mut remap: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        remap.push(u32::MAX);
    }
    let mut elided_any = false;
    for i in 0..func.ops.len() {
        if is_protected(func, i) {
            continue;
        }
        let MOp::Mov { dst: MReg::V(d), src: MReg::V(s), wide: true } = op_at(func, i) else {
            continue;
        };
        if d == s {
            continue;
        }
        let di = to_idx(d.0);
        let si = to_idx(s.0);
        if def_counts.get(di).copied().unwrap_or(0) != 1
            || def_counts.get(si).copied().unwrap_or(0) != 1
            || at(&is_phi, di, true)
            || at(&is_phi, si, true)
            || !func.vreg_class.get(di).copied().is_some_and(VRegClass::is_gpr)
            || !func.vreg_class.get(si).copied().is_some_and(VRegClass::is_gpr)
        {
            continue;
        }
        let mut cur = s.0;
        let mut hops: usize = 0;
        let mut safe_chain = true;
        while hops < MAX_THREAD_HOPS {
            if cur == d.0 {
                safe_chain = false;
                break;
            }
            let next = remap.get(to_idx(cur)).copied().unwrap_or(u32::MAX);
            if next == u32::MAX {
                break;
            }
            cur = next;
            hops += 1;
        }
        if !safe_chain || hops >= MAX_THREAD_HOPS {
            continue;
        }
        set(&mut remap, di, s.0);
        mark_deleted(func, i);
        stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
        elided_any = true;
    }

    if elided_any {
        for op in func.ops.iter_mut() {
            remap_op_vreg_uses(op, &mut func.copy_pairs, &remap);
        }
    }
}

// -----------------------------------------------------------------------------
// Pass 1: Peepholes, `MovImm` CSE, and Local Copy Propagation
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ImmCseSlot {
    valid: bool,
    reg: MReg,
    imm: u64,
    wide: bool,
}

impl ImmCseSlot {
    const EMPTY: Self = Self { valid: false, reg: MReg::XZR, imm: 0, wide: false };
}

/// Per-block tracker of GPRs (`PReg::Gpr(0..=30)` and SSA `VReg`s) whose upper 32 bits
/// (`bits 32..63`) are provably zero. Uses a generation stamp for O(1) per-block resets.
struct ZeroUpper32<'a> {
    phys_mask: u32,
    vreg_gen: Vec<u32, Alloc<'a>>,
    cur_gen: u32,
}

impl<'a> ZeroUpper32<'a> {
    fn new(func: &MFunc<'_>, alloc: Alloc<'a>) -> Self {
        let mut vreg_gen: Vec<u32, Alloc<'a>> = if func.is_ssa && !func.vreg_class.is_empty() {
            Vec::with_capacity_in(func.vreg_class.len(), alloc)
        } else {
            Vec::new_in(alloc)
        };
        if func.is_ssa {
            for _ in 0..func.vreg_class.len() {
                vreg_gen.push(0);
            }
        }
        Self { phys_mask: 0, vreg_gen, cur_gen: 0 }
    }

    #[inline]
    fn reset_block(&mut self) {
        self.phys_mask = 0;
        let next = self.cur_gen.wrapping_add(1);
        if next == 0 {
            for slot in self.vreg_gen.iter_mut() {
                *slot = 0;
            }
            self.cur_gen = 1;
        } else {
            self.cur_gen = next;
        }
    }

    #[inline]
    fn is_zero_hi(&self, reg: MReg) -> bool {
        match reg {
            MReg::P(PReg::Xzr) => true,
            MReg::P(PReg::Gpr(r)) if r <= 30 => (self.phys_mask & (1u32 << u32::from(r))) != 0,
            MReg::V(v) => self.vreg_gen.get(to_idx(v.0)).copied().unwrap_or(0) == self.cur_gen,
            MReg::P(PReg::Gpr(_) | PReg::Sp | PReg::V(_) | PReg::Nzcv) => false,
        }
    }

    #[inline]
    fn set_zero_hi(&mut self, reg: MReg) {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => {
                self.phys_mask |= 1u32 << u32::from(r);
            }
            MReg::V(v) => {
                if let Some(slot) = self.vreg_gen.get_mut(to_idx(v.0)) {
                    *slot = self.cur_gen;
                }
            }
            MReg::P(PReg::Gpr(_) | PReg::Sp | PReg::Xzr | PReg::V(_) | PReg::Nzcv) => {}
        }
    }

    #[inline]
    fn clear_zero_hi(&mut self, reg: MReg) {
        match reg {
            MReg::P(PReg::Gpr(r)) if r <= 30 => {
                self.phys_mask &= !(1u32 << u32::from(r));
            }
            MReg::V(v) => {
                if let Some(slot) = self.vreg_gen.get_mut(to_idx(v.0)) {
                    *slot = 0;
                }
            }
            MReg::P(PReg::Gpr(_) | PReg::Sp | PReg::Xzr | PReg::V(_) | PReg::Nzcv) => {}
        }
    }
}

/// Pass 1: intra-MBB peepholes, known-zero-upper-32 tracking & redundant `mov wR, wR` elision,
/// `MovImm` CSE, wide and narrow-32 local copy propagation, `SubImm #1`+`Csinc` commute,
/// `SubImm`-into-`CmpImm` fusion, flag fusion, `Ubfm` LSR+UBFX fusion, and single-use shift-into-
/// ALU fusion (works identically on both SSA `VReg`s and post-RA `PReg`s).
fn mopt_peepholes_and_cse<'a>(func: &mut MFunc<'a>, alloc: Alloc<'a>, stats: &mut MOptStats) {
    let mut vreg_uses = compute_vreg_use_counts(func, alloc);
    let mut zero_hi = ZeroUpper32::new(func, alloc);
    let live_out_gprs = if !func.is_ssa {
        compute_post_ra_live_out_gprs(func, alloc)
    } else {
        None
    };
    let n_order = func.block_order.len();

    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        if start >= end {
            continue;
        }
        let live_out_mask = live_out_gprs
            .as_ref()
            .and_then(|v| v.get(to_idx(blk.0)).copied());

        // 1a. Dead self-`Mov` removal (64-bit `Mov`, `MovSp`, `FpAlu::Mov`, `VecAlu::Mov`) +
        //     known-zero-upper-32 tracking & redundant `mov wR, wR` elimination (pre- & post-RA).
        for i in start..end {
            if is_protected(func, i) {
                continue;
            }
            let op_i = op_at(func, i);
            let is_self_mov = match op_i {
                MOp::Mov { dst, src, wide: true } => dst == src,
                MOp::MovSp { dst, src } => dst == src,
                MOp::FpAlu { op: FpAluOp::Mov, dst, rn, .. } => dst == rn,
                MOp::VecAlu { op: VecAluOp::Mov, dst, rn, .. } => dst == rn,
                _ => false,
            };
            if is_self_mov {
                dec_op_vreg_uses(&op_i, &func.copy_pairs, &mut vreg_uses);
                mark_deleted(func, i);
                stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
            }
        }
        peephole_known_zero_upper32(func, start, end, &mut zero_hi, &mut vreg_uses, stats);
        if !func.is_ssa {
            peephole_dead_narrow32_mov_post_ra(func, start, end, stats);
        }

        // 1b. Intra-MBB `MovImm` CSE (reuses an unmodified register holding `(imm, wide)`).
        peephole_mov_imm_cse(func, start, end, &mut vreg_uses, stats);

        // 1c. Local wide (`Mov { wide: true }`/`MovSp`) and narrow-32 (`Mov { wide: false }` into
        //     32-bit-only consumers) copy propagation (<= 24 insts) and dead-`Mov` removal.
        let copy_before_peep = stats.peepholes;
        let copy_before_mov = stats.movs_eliminated;
        peephole_local_copy_prop(func, start, end, &mut vreg_uses, stats);
        if stats.peepholes != copy_before_peep || stats.movs_eliminated != copy_before_mov {
            // Re-sweep known-zero-upper-32 & post-RA dead `mov wR, wR` to catch any narrow
            // truncation newly exposed by copy propagation.
            peephole_known_zero_upper32(func, start, end, &mut zero_hi, &mut vreg_uses, stats);
            if !func.is_ssa {
                peephole_dead_narrow32_mov_post_ra(func, start, end, stats);
            }
        }

        // 1c-2. SSA `cset` (`Csinc(Xzr, Xzr)`) + `AddReg` -> `Csinc(other, other)` (`cinc`).
        peephole_fuse_cset_add(func, start, end, &mut vreg_uses, stats);

        // 1d. `SubImm #1` + `Csinc` (CINC) commutation + `CmpImm #0` deletion (Z-only).
        peephole_commute_sub1_csinc(func, start, end, &mut vreg_uses, stats);

        // 1e. `SubImm` + `CmpImm` fusion (ONLY when `dst != rn`!).
        peephole_sink_sub_into_cmp(func, start, end, &mut vreg_uses, stats);

        // 1f. Flag fusion (`AddImm`/`SubImm`/`AddReg`/`SubReg` + `CmpImm(rn, 0)`, N/Z-only).
        peephole_fuse_addsub_cmp_zero(func, start, end, &mut vreg_uses, stats);

        // 1g. `Ubfm` LSR + `Ubfm` UBFX fusion (`sh + lsb + w <= width`).
        peephole_fuse_lsr_ubfx(func, start, end, &mut vreg_uses, stats);

        // 1h. Single-use `Ubfm`/`Sbfm` shift-into-ALU/`Mvn` fusion (`AddReg`/`SubReg`/`LogicalReg`/`Mvn`),
        //     including zero-upper-32 narrow `LSR` into 64-bit ALU.
        peephole_fuse_shift_into_alu(func, start, end, &zero_hi, live_out_mask, &mut vreg_uses, stats);

        // 1h-2. Single-use `uxtb`/`uxth`/`uxtw` (`Ubfm` or `And #0xff/#0xffff`) + 64-bit `AddReg`/`SubReg`
        //       -> `AddExt`/`SubExt` (`add/sub xD, xN, wM, uxtb/uxth/uxtw`).
        peephole_fuse_uxt_into_addsub(func, start, end, live_out_mask, &mut vreg_uses, stats);

        // 1i. SSA `neg` (optional shift) + `AddReg` -> `SubReg` fusion.
        peephole_fuse_neg_add(func, start, end, &mut vreg_uses, stats);

        // 1j. `CmpImm(rn, 0) + BCond(Eq|Ls|Ne|Hi)` -> `Cbz`/`Cbnz` (frees `NZCV`, unlocks Pass 5).
        peephole_fuse_cmp_zero_branch(func, start, end, &mut vreg_uses, stats);

        // 1k. `MovImm(0|1) + Csel` -> `Csel(..., Xzr)` / `Csinc(..., Xzr)`.
        peephole_fold_const_csel(func, start, end, live_out_mask, &mut vreg_uses, stats);

        // 1l. Post-RA liveness-backed constant-copy propagation, backward single-def copy folding,
        //     and redundant `uxtb`/`uxth` before `strb`/`strh` elimination.
        if !func.is_ssa {
            peephole_post_ra_const_copy_prop(func, start, end, live_out_mask, stats);
            peephole_post_ra_backward_copy_fold(func, start, end, live_out_mask, stats);
            peephole_post_ra_uxtb_str(func, start, end, live_out_mask, stats);
        }
    }
}

/// Post-RA (`!func.is_ssa`) intra-MBB dead 32-bit truncation elision: deletes an unpinned,
/// noteless `MOp::Mov { dst: P(Gpr(r)), src: P(Gpr(r)), wide: false }` whenever scanning forward
/// in the same block proves `r` is definitely overwritten before block exit (or dead at `Ret`)
/// and every intervening read of `r` strictly reads only the low 32 bits.
fn peephole_dead_narrow32_mov_post_ra(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    stats: &mut MOptStats,
) {
    if func.is_ssa {
        return;
    }
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let MOp::Mov { dst: MReg::P(PReg::Gpr(r)), src: MReg::P(PReg::Gpr(r2)), wide: false } =
            op_at(func, i)
        else {
            continue;
        };
        if r != r2 || r > 30 {
            continue;
        }
        let reg = MReg::P(PReg::Gpr(r));
        let mut definitely_dead_or_overwritten = false;

        for k in (i + 1)..end {
            let op_k = op_at(func, k);
            if matches!(op_k, MOp::Nop) {
                continue;
            }
            if matches!(op_k, MOp::Ret) {
                if r < 30 && (func.ret_live_gprs & (1u32 << u32::from(r))) == 0 {
                    definitely_dead_or_overwritten = true;
                }
                break;
            }
            if is_control_or_call(&op_k) {
                break;
            }
            // Check reads BEFORE defs so `add wR, w26, wR` (32-bit read + overwrite) succeeds,
            // while `add xR, x26, xR` (64-bit read + overwrite) is rejected.
            if !op_uses_reg_only_low32(&op_k, &func.copy_pairs, reg) {
                break;
            }
            if inst_defines_reg(func, k, reg) {
                definitely_dead_or_overwritten = true;
                break;
            }
        }

        if definitely_dead_or_overwritten {
            mark_deleted(func, i);
            stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
        }
    }
}

/// Lightweight per-block cleanup run after Pass 2 whenever `mopt_redundant_loads` forwarded at
/// least one load into a `Mov`: elides any dead `mov wR, wR` (e.g. from `str wR -> ldr wR`) and
/// locally propagates forwarded `Mov`s.
fn mopt_post_load_forward_cleanup<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
    stats: &mut MOptStats,
) {
    let mut vreg_uses = compute_vreg_use_counts(func, alloc);
    let mut zero_hi = ZeroUpper32::new(func, alloc);
    let n_order = func.block_order.len();
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        if start >= end {
            continue;
        }
        peephole_known_zero_upper32(func, start, end, &mut zero_hi, &mut vreg_uses, stats);
        if !func.is_ssa {
            peephole_dead_narrow32_mov_post_ra(func, start, end, stats);
        }
        peephole_local_copy_prop(func, start, end, &mut vreg_uses, stats);
        if !func.is_ssa {
            peephole_dead_narrow32_mov_post_ra(func, start, end, stats);
        }
    }
}

/// Tracks registers whose upper 32 bits (`bits 32..63`) are provably zero within `[start, end)`,
/// elides redundant `Mov { dst == src, wide: false }` (e.g. `mov w12, w12` after `lsr x12, x, #32`),
/// and upgrades `Mov { dst != src, wide: false }` with zero-upper-32 `src` to `wide: true` so wide
/// copy-propagation and RA coalescing can eliminate it in both SSA and post-RA.
fn peephole_known_zero_upper32(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    state: &mut ZeroUpper32<'_>,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    state.reset_block();
    let u32_max = u64::from(u32::MAX);

    for i in start..end {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }

        if let MOp::Mov { dst, src, wide: false } = op {
            if !is_protected(func, i) && state.is_zero_hi(src) {
                if dst == src {
                    dec_op_vreg_uses(&op, &func.copy_pairs, vreg_uses);
                    mark_deleted(func, i);
                    stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
                    continue;
                }
                set(&mut func.ops, i, MOp::Mov { dst, src, wide: true });
                stats.peepholes = stats.peepholes.saturating_add(1);
            }
            state.set_zero_hi(dst);
            continue;
        }

        match op {
            MOp::Mov { dst, src, wide: true } => {
                if !is_protected(func, i) && dst == src {
                    dec_op_vreg_uses(&op, &func.copy_pairs, vreg_uses);
                    mark_deleted(func, i);
                    stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
                    continue;
                }
                if state.is_zero_hi(src) {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::Mov { wide: false, .. } => {}
            MOp::MovSp { dst, src } => {
                if !is_protected(func, i) && dst == src {
                    dec_op_vreg_uses(&op, &func.copy_pairs, vreg_uses);
                    mark_deleted(func, i);
                    stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
                    continue;
                }
                if state.is_zero_hi(src) {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::MovImm { dst, imm, wide } => {
                if !wide || imm <= u32_max {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::LogicalImm { dst, imm, kind, wide, .. } => {
                if !wide || (matches!(kind, LogicalKind::And | LogicalKind::Ands) && imm <= u32_max)
                {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::LogicalReg { dst, rn, rm, shift, invert_rm, kind, wide } => {
                if !wide
                    || (matches!(kind, LogicalKind::And | LogicalKind::Ands)
                        && !invert_rm
                        && shift.is_none()
                        && (state.is_zero_hi(rn) || state.is_zero_hi(rm)))
                {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::Ubfm { dst, immr, imms, wide, .. } => {
                let zero_hi = !wide
                    || (immr <= 63
                        && imms <= 63
                        && ((imms >= immr && (imms - immr) <= 31)
                            || (imms < immr && (immr - imms) > 32)));
                if zero_hi {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::Clz { dst, .. } => {
                // `clz` always yields `0..=64 <= u32::MAX` for both 32-bit and 64-bit forms.
                state.set_zero_hi(dst);
            }
            MOp::Csel { dst, rn, rm, wide, .. } => {
                if !wide || (state.is_zero_hi(rn) && state.is_zero_hi(rm)) {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::AddImm { dst, wide, .. }
            | MOp::SubImm { dst, wide, .. }
            | MOp::AddReg { dst, wide, .. }
            | MOp::SubReg { dst, wide, .. }
            | MOp::AddExt { dst, wide, .. }
            | MOp::SubExt { dst, wide, .. }
            | MOp::Mvn { dst, wide, .. }
            | MOp::ShiftVar { dst, wide, .. }
            | MOp::Sbfm { dst, wide, .. }
            | MOp::Bfxil { dst, wide, .. }
            | MOp::Extr { dst, wide, .. }
            | MOp::Rbit { dst, wide, .. }
            | MOp::Rev { dst, wide, .. }
            | MOp::Mul { dst, wide, .. }
            | MOp::Madd { dst, wide, .. }
            | MOp::Msub { dst, wide, .. }
            | MOp::Udiv { dst, wide, .. }
            | MOp::Sdiv { dst, wide, .. }
            | MOp::Umin { dst, wide, .. }
            | MOp::Umax { dst, wide, .. }
            | MOp::Smin { dst, wide, .. }
            | MOp::Smax { dst, wide, .. }
            | MOp::Csinc { dst, wide, .. }
            | MOp::Csinv { dst, wide, .. }
            | MOp::Csneg { dst, wide, .. } => {
                if !wide {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::Ldr { dst, addr, size, signed } => {
                if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                    state.clear_zero_hi(base);
                }
                if !signed && matches!(size, MemSize::B8 | MemSize::H16 | MemSize::W32) {
                    state.set_zero_hi(dst);
                } else if !size.is_fp_or_vec() {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::Ldp { dst0, dst1, base, size, post_idx, pre_idx, .. } => {
                if post_idx || pre_idx {
                    state.clear_zero_hi(base);
                }
                if size == MemSize::W32 {
                    state.set_zero_hi(dst0);
                    state.set_zero_hi(dst1);
                } else if !size.is_fp_or_vec() {
                    state.clear_zero_hi(dst0);
                    state.clear_zero_hi(dst1);
                }
            }
            MOp::Str { addr, .. } => {
                if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                    state.clear_zero_hi(base);
                }
            }
            MOp::Stp { base, post_idx, pre_idx, .. } => {
                if post_idx || pre_idx {
                    state.clear_zero_hi(base);
                }
            }
            MOp::FpAlu { op: fp_op, dst, double, .. } => match fp_op {
                FpAluOp::MovToGpr => {
                    if !double {
                        state.set_zero_hi(dst);
                    } else {
                        state.clear_zero_hi(dst);
                    }
                }
                FpAluOp::Fcvtzs { int_wide } | FpAluOp::Fcvtzu { int_wide } => {
                    if !int_wide {
                        state.set_zero_hi(dst);
                    } else {
                        state.clear_zero_hi(dst);
                    }
                }
                _ => {}
            },
            MOp::VecAlu { op: VecAluOp::UmovLane { .. }, dst, elem_size, .. } => {
                if elem_size <= 2 {
                    state.set_zero_hi(dst);
                } else {
                    state.clear_zero_hi(dst);
                }
            }
            MOp::VecAlu { .. } => {}
            MOp::Umulh { dst, .. }
            | MOp::Smulh { dst, .. }
            | MOp::CarryAddSub { dst, .. }
            | MOp::Adrp { dst }
            | MOp::AddLo12 { dst, .. }
            | MOp::AdrTable { dst, .. } => {
                state.clear_zero_hi(dst);
            }
            MOp::Bl | MOp::Blr { .. } => {
                // Calls clobber physical GPRs; SSA `VReg`s remain immutable across calls.
                state.phys_mask = 0;
            }
            MOp::ParallelCopy { start: copy_start, len } => {
                let s = to_idx(copy_start);
                let e = s.saturating_add(usize::from(len)).min(func.copy_pairs.len());
                if let Some(pairs) = func.copy_pairs.get(s..e) {
                    for &(dst, _) in pairs {
                        state.clear_zero_hi(dst);
                    }
                }
            }
            MOp::RawWord { defs_gpr, .. } => {
                state.phys_mask &= !defs_gpr;
            }
            MOp::CmpImm { .. }
            | MOp::CmpReg { .. }
            | MOp::TstImm { .. }
            | MOp::TstReg { .. }
            | MOp::CcmpImm { .. }
            | MOp::CcmpReg { .. }
            | MOp::FpCmp { .. }
            | MOp::Fcsel { .. }
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
            | MOp::Nop => {}
        }
    }
}

fn peephole_mov_imm_cse(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    let mut table = [ImmCseSlot::EMPTY; IMM_CSE_SLOTS];
    let mut next_evict: usize = 0;

    for i in start..end {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }
        if let MOp::MovImm { dst, imm, wide } = op {
            let imm_norm = if wide { imm } else { imm & 0xffff_ffff };
            let mut found_reg: Option<MReg> = None;
            for slot in &table {
                if slot.valid && slot.imm == imm_norm && slot.wide == wide {
                    found_reg = Some(slot.reg);
                    if slot.reg == dst {
                        break;
                    }
                }
            }

            if !is_protected(func, i)
                && is_trackable_reg(dst)
                && let Some(existing) = found_reg
            {
                if existing == dst {
                    dec_op_vreg_uses(&op, &func.copy_pairs, vreg_uses);
                    mark_deleted(func, i);
                    stats.peepholes = stats.peepholes.saturating_add(1);
                    continue;
                }
                let new_op = MOp::Mov { dst, src: existing, wide: true };
                dec_op_vreg_uses(&op, &func.copy_pairs, vreg_uses);
                set(&mut func.ops, i, new_op);
                set(&mut func.flags, i, MInstFlags::NONE);
                inc_op_vreg_uses(&new_op, &func.copy_pairs, vreg_uses);
                stats.peepholes = stats.peepholes.saturating_add(1);
            }

            // Invalidate prior binding for `dst`, then record `(dst, imm_norm, wide)`.
            for slot in &mut table {
                if slot.valid && slot.reg == dst {
                    slot.valid = false;
                }
            }
            if is_trackable_reg(dst) {
                let mut inserted = false;
                for slot in &mut table {
                    if !slot.valid {
                        *slot = ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide };
                        inserted = true;
                        break;
                    }
                }
                if !inserted {
                    set(
                        &mut table,
                        next_evict % IMM_CSE_SLOTS,
                        ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide },
                    );
                    next_evict = next_evict.wrapping_add(1);
                }
            }
            continue;
        }

        if is_call_or_mem_barrier(func, i) || is_control_or_call(&op) {
            for slot in &mut table {
                slot.valid = false;
            }
            continue;
        }
        for slot in &mut table {
            if slot.valid && inst_defines_reg(func, i, slot.reg) {
                slot.valid = false;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyKind {
    /// 64-bit `Mov { wide: true }` or `MovSp`: preserves all 64 bits.
    Wide,
    /// 32-bit `Mov { wide: false }`: only safe to propagate into strictly 32-bit consumers.
    Narrow32,
}

fn peephole_local_copy_prop(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        let op_i = op_at(func, i);
        let (dst, src, kind) = match op_i {
            MOp::Mov { dst, src, wide: true } if dst != src && is_trackable_reg(dst) => {
                (dst, src, CopyKind::Wide)
            }
            MOp::MovSp { dst, src } if dst != src && is_trackable_reg(dst) => {
                (dst, src, CopyKind::Wide)
            }
            MOp::Mov { dst, src, wide: false }
                if !is_protected(func, i)
                    && dst != src
                    && is_trackable_reg(dst)
                    && src != MReg::SP =>
            {
                (dst, src, CopyKind::Narrow32)
            }
            _ => continue,
        };

        let mut unrewritable_use_seen = false;
        let mut dst_redefined = false;
        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));

        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            let stop_after_rewrite = is_control_or_call(&op_j);
            let uses_dst = match op_j {
                MOp::Bl | MOp::Ret | MOp::RawWord { .. } if dst.is_virtual() => false,
                _ => inst_uses_reg(func, j, dst),
            };

            if uses_dst {
                if !is_protected(func, j) {
                    let mut candidate = op_j;
                    let res = match kind {
                        CopyKind::Wide => rewrite_gpr_uses_in_op(&mut candidate, dst, src),
                        CopyKind::Narrow32 => rewrite_narrow32_uses_in_op(&mut candidate, dst, src),
                    };
                    if res.rewrote_any {
                        dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                        set(&mut func.ops, j, candidate);
                        inc_op_vreg_uses(&candidate, &func.copy_pairs, vreg_uses);
                        stats.peepholes = stats.peepholes.saturating_add(1);
                    }
                    if res.had_unrewritable_use {
                        unrewritable_use_seen = true;
                    }
                } else {
                    unrewritable_use_seen = true;
                }
            }

            if inst_defines_reg(func, j, dst) {
                dst_redefined = true;
                break;
            }
            if inst_defines_reg(func, j, src) || stop_after_rewrite {
                break;
            }
        }

        // Delete the copy if it is unprotected and `dst` is now provably dead!
        if !is_protected(func, i) && !unrewritable_use_seen {
            let dead = if func.is_ssa {
                if let MReg::V(v) = dst {
                    vreg_uses.get(to_idx(v.0)).copied().unwrap_or(1) == 0
                } else {
                    dst_redefined
                }
            } else {
                dst_redefined || is_reg_dead_after(func, end, i, dst, vreg_uses)
            };
            if dead {
                dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                mark_deleted(func, i);
                stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
            }
        }
    }
}

/// Commutes `SubImm { dst: rd, rn, imm12: 1, shift12: false, sets_flags: false }` +
/// `Csinc { dst: cinc_rd, rn: rd, rm: rd, cond }` (CINC) into `Csinc` + `SubImm #1`, and when a
/// matching `CmpImm { rn: cinc_rd, imm12: 0 }` follows whose consumers inspect ONLY `Z` (`Eq`/`Ne`),
/// upgrades the sunk `SubImm` to `sets_flags: true` and deletes `CmpImm`.
fn peephole_commute_sub1_csinc(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let MOp::SubImm { dst: rd, rn, imm12: 1, shift12: false, wide, sets_flags: false } =
            op_at(func, i)
        else {
            continue;
        };
        if is_sp_or_xzr(rd) || is_sp_or_xzr(rn) {
            continue;
        }

        // 1. Search forward for `Csinc { dst: cinc_rd, rn: rd, rm: rd, cond, wide }`.
        let mut found_j: Option<(usize, MReg, MCond)> = None;
        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for k in (i + 1)..scan_end {
            let op_k = op_at(func, k);
            if matches!(op_k, MOp::Nop) {
                continue;
            }
            if !is_protected(func, k)
                && let MOp::Csinc { dst: cinc_rd, rn: c_rn, rm: c_rm, cond, wide: c_wide } = op_k
                && c_rn == rd
                && c_rm == rd
                && c_wide == wide
                && !matches!(cond, MCond::Al | MCond::Nv)
                && !is_sp_or_xzr(cinc_rd)
            {
                found_j = Some((k, cinc_rd, cond));
                break;
            }
            if op_k.writes_flags()
                || is_control_or_call(&op_k)
                || is_call_or_mem_barrier(func, k)
                || matches!(op_k, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_defines_reg(func, k, rd)
                || inst_defines_reg(func, k, rn)
                || inst_uses_reg(func, k, rd)
            {
                break;
            }
        }
        let Some((j, cinc_rd, cond)) = found_j else {
            continue;
        };

        // If `cinc_rd != rd` (e.g. SSA), ensure `rd` has no other uses outside `j` (so `rd` can
        // safely hold the hoisted `Csinc` intermediate without breaking any other reader of `rd`).
        if cinc_rd != rd {
            let rd_sole_use_is_j = if func.is_ssa {
                if let MReg::V(v) = rd {
                    // `Csinc` uses `rd` twice (`rn` and `rm`), so sole-use count is 2.
                    vreg_uses.get(to_idx(v.0)).copied().unwrap_or(0) == 2
                } else {
                    is_reg_dead_after(func, end, j, rd, vreg_uses)
                }
            } else {
                is_reg_dead_after(func, end, j, rd, vreg_uses)
            };
            if !rd_sole_use_is_j {
                continue;
            }
        }

        // 2. Check for a following `CmpImm { rn: cinc_rd, imm12: 0, shift12: false, wide }`
        //    whose flag consumers inspect ONLY `Z` (`Eq`/`Ne`).
        let mut cmp_to_delete: Option<usize> = None;
        let cmp_scan_end = end.min(j.saturating_add(PEEPHOLE_WINDOW));
        for c in (j + 1)..cmp_scan_end {
            let op_c = op_at(func, c);
            if matches!(op_c, MOp::Nop) {
                continue;
            }
            if !is_protected(func, c)
                && let MOp::CmpImm { rn: cmp_rn, imm12: 0, shift12: false, wide: cmp_wide } = op_c
                && cmp_rn == cinc_rd
                && cmp_wide == wide
            {
                if flags_consumers_satisfy(func, c, end, cond_only_z) {
                    cmp_to_delete = Some(c);
                }
                break;
            }
            if op_c.reads_flags()
                || op_c.writes_flags()
                || is_control_or_call(&op_c)
                || is_call_or_mem_barrier(func, c)
                || matches!(op_c, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_defines_reg(func, c, cinc_rd)
            {
                break;
            }
        }

        // Only commute when we either eliminate `CmpImm #0` or `cinc_rd == rd`.
        if cmp_to_delete.is_none() && cinc_rd != rd {
            continue;
        }

        // 3. Apply the sound commute:
        //    - `[i]` <- `Csinc { dst: rd, rn, rm: rn, cond, wide }` (inherits `vflag_use[j]`)
        //    - `[j]` <- `SubImm { dst: cinc_rd, rn: rd, imm12: 1, sets_flags: cmp_to_delete.is_some() }`
        let old_i = op_at(func, i);
        let old_j = op_at(func, j);
        dec_op_vreg_uses(&old_i, &func.copy_pairs, vreg_uses);
        dec_op_vreg_uses(&old_j, &func.copy_pairs, vreg_uses);

        let new_i = MOp::Csinc { dst: rd, rn, rm: rn, cond, wide };
        let new_j = MOp::SubImm {
            dst: cinc_rd,
            rn: rd,
            imm12: 1,
            shift12: false,
            wide,
            sets_flags: cmp_to_delete.is_some(),
        };

        let vf_use_j = vflag_use_at(func, j);
        set(&mut func.ops, i, new_i);
        set(&mut func.vflag_use, i, vf_use_j);
        set(&mut func.vflag_def, i, NO_VFLAG);

        set(&mut func.ops, j, new_j);
        set(&mut func.vflag_use, j, NO_VFLAG);

        inc_op_vreg_uses(&new_i, &func.copy_pairs, vreg_uses);
        inc_op_vreg_uses(&new_j, &func.copy_pairs, vreg_uses);

        if let Some(c) = cmp_to_delete {
            let cmp_vdef = vflag_def_at(func, c);
            let op_c = op_at(func, c);
            dec_op_vreg_uses(&op_c, &func.copy_pairs, vreg_uses);
            mark_deleted(func, c);
            set(&mut func.vflag_def, j, cmp_vdef);
        } else {
            set(&mut func.vflag_def, j, NO_VFLAG);
        }
        stats.peepholes = stats.peepholes.saturating_add(1);
    }
}

/// Fuses `SubImm { dst, rn, imm12, shift12, wide, sets_flags: false }` +
/// `CmpImm { rn: jrn, imm12, shift12, wide }` into a single `SubImm { sets_flags: true }` at the
/// `CmpImm`'s site ONLY when **`dst != rn`** (mandatory: when `dst == rn`, `sub rn, rn, #imm`
/// mutates `rn` BEFORE `cmp rn, #imm`, so `cmp` compares `old_rn - imm` against `imm`!).
fn peephole_sink_sub_into_cmp(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let op_i = op_at(func, i);
        let MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags: false } = op_i else {
            continue;
        };
        // CRITICAL: `dst != rn` prevents self-clobber miscompilation; `dst` must also be `GprOrZr`.
        if dst == rn || is_sp_or_xzr(dst) {
            continue;
        }
        if vflag_def_at(func, i) != NO_VFLAG || vflag_use_at(func, i) != NO_VFLAG {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j)
                && vflag_use_at(func, j) == NO_VFLAG
                && let MOp::CmpImm { rn: jrn, imm12: jimm, shift12: jsh, wide: jwide } = op_j
                && jrn == rn
                && jimm == imm12
                && jsh == shift12
                && jwide == wide
            {
                // Delete `SubImm` at `i` and rewrite `CmpImm` at `j` in place, preserving `vflag_def[j]`.
                dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                mark_deleted(func, i);

                let fused = MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags: true };
                set(&mut func.ops, j, fused);
                inc_op_vreg_uses(&fused, &func.copy_pairs, vreg_uses);
                stats.peepholes = stats.peepholes.saturating_add(1);
                break;
            }

            if is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || flags_at(func, j).contains(MInstFlags::MAY_TRAP)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_touches_reg(func, j, dst)
                || inst_defines_reg(func, j, rn)
            {
                break;
            }
        }
    }
}

/// Fuses non-flag-setting `AddImm`/`SubImm`/`AddReg`/`SubReg` + `CmpImm(prev_dst, 0)` into
/// `sets_flags: true` when no intervening flag read/write or `prev_dst` def occurs and all
/// downstream flag consumers in the block read ONLY `N`/`Z` (`Eq`/`Ne`/`Mi`/`Pl`), accurately
/// transferring `vflag_def`.
fn peephole_fuse_addsub_cmp_zero(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) || vflag_def_at(func, i) != NO_VFLAG || vflag_use_at(func, i) != NO_VFLAG
        {
            continue;
        }
        let (prev_dst, wide) = match op_at(func, i) {
            MOp::AddImm { dst, wide, sets_flags: false, .. }
            | MOp::SubImm { dst, wide, sets_flags: false, .. }
            | MOp::AddReg { dst, wide, sets_flags: false, .. }
            | MOp::SubReg { dst, wide, sets_flags: false, .. } => (dst, wide),
            _ => continue,
        };
        // `ADDS`/`SUBS` destination role is `GprOrZr` (never `SP`).
        if is_sp_or_xzr(prev_dst) {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j)
                && let MOp::CmpImm { rn, imm12: 0, shift12: false, wide: cmp_wide } = op_j
                && rn == prev_dst
                && cmp_wide == wide
            {
                if flags_consumers_satisfy(func, j, end, cond_only_nz) {
                    let cmp_vdef = vflag_def_at(func, j);
                    if let Some(op_i_mut) = func.ops.get_mut(i) {
                        match op_i_mut {
                            MOp::AddImm { sets_flags, .. }
                            | MOp::SubImm { sets_flags, .. }
                            | MOp::AddReg { sets_flags, .. }
                            | MOp::SubReg { sets_flags, .. } => {
                                *sets_flags = true;
                            }
                            _ => {}
                        }
                    }
                    set(&mut func.vflag_def, i, cmp_vdef);
                    dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                    mark_deleted(func, j);
                    stats.peepholes = stats.peepholes.saturating_add(1);
                }
                break;
            }

            if op_j.writes_flags()
                || op_j.reads_flags()
                || is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || inst_defines_reg(func, j, prev_dst)
            {
                break;
            }
        }
    }
}

/// Fuses `Ubfm` LSR (`lsr rd, rn, #sh`, i.e. `imms == width - 1`) + `Ubfm` UBFX
/// (`ubfx rd2, rd, #lsb, #w`, i.e. `imms >= immr`) into a single `Ubfm` when
/// `sh + lsb + w <= width` and `rd` is dead after the `ubfx` (or `rd == rd2`).
fn peephole_fuse_lsr_ubfx(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let op_i = op_at(func, i);
        let MOp::Ubfm { dst: rd, rn, immr: sh, imms, wide } = op_i else {
            continue;
        };
        let width: u8 = if wide { 64 } else { 32 };
        let max_bit = width - 1;
        if imms != max_bit || !is_trackable_reg(rd) {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j)
                && let MOp::Ubfm { dst: rd2, rn: jrn, immr: lsb, imms: j_imms, wide: j_wide } = op_j
                && jrn == rd
                && j_wide == wide
                && j_imms >= lsb
            {
                let w = u16::from(j_imms - lsb) + 1;
                let total = u16::from(sh) + u16::from(lsb) + w;
                if total <= u16::from(width) {
                    let rd_dead = rd == rd2
                        || if func.is_ssa {
                            if let MReg::V(v) = rd {
                                vreg_uses.get(to_idx(v.0)).copied().unwrap_or(0) == 1
                            } else {
                                is_reg_dead_after(func, end, j, rd, vreg_uses)
                            }
                        } else {
                            is_reg_dead_after(func, end, j, rd, vreg_uses)
                        };
                    if rd_dead {
                        let new_immr = sh.saturating_add(lsb);
                        let new_imms = sh.saturating_add(j_imms);
                        let fused = MOp::Ubfm {
                            dst: rd2,
                            rn,
                            immr: new_immr,
                            imms: new_imms,
                            wide,
                        };
                        dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                        dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                        mark_deleted(func, i);
                        set(&mut func.ops, j, fused);
                        inc_op_vreg_uses(&fused, &func.copy_pairs, vreg_uses);
                        stats.peepholes = stats.peepholes.saturating_add(1);
                    }
                }
                break;
            }

            if is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_touches_reg(func, j, rd)
                || inst_defines_reg(func, j, rn)
            {
                break;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct DecodedShift {
    dst: MReg,
    src: MReg,
    shift: Option<MShift>,
    wide: bool,
}

/// Decodes pure immediate shifts encoded as `Ubfm` (`LSR`/`LSL`) or `Sbfm` (`ASR`).
#[inline]
fn decode_const_shift(op: MOp) -> Option<DecodedShift> {
    match op {
        MOp::Ubfm { dst, rn, immr, imms, wide } => {
            let bits: u8 = if wide { 64 } else { 32 };
            let max_bit = bits - 1;
            if imms == max_bit && immr <= max_bit {
                let shift = if immr == 0 { None } else { Some(MShift::Lsr(immr)) };
                Some(DecodedShift { dst, src: rn, shift, wide })
            } else if imms < max_bit && immr == imms.saturating_add(1) {
                let amt = bits - immr;
                Some(DecodedShift { dst, src: rn, shift: Some(MShift::Lsl(amt)), wide })
            } else {
                None
            }
        }
        MOp::Sbfm { dst, rn, immr, imms, wide } => {
            let max_bit: u8 = if wide { 63 } else { 31 };
            if imms == max_bit && immr <= max_bit {
                let shift = if immr == 0 { None } else { Some(MShift::Asr(immr)) };
                Some(DecodedShift { dst, src: rn, shift, wide })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Fuses single-use immediate shifts (`Ubfm` `LSR`/`LSL`, `Sbfm` `ASR`) directly into a following
/// `AddReg`, `SubReg`, `LogicalReg`, or `Mvn` with `shift: None` and matching width (or a 32-bit
/// `LSR` of a known-zero-upper-32 SSA `VReg` directly into a 64-bit `AddReg`/`SubReg`/
/// `LogicalReg`, swapping `rn`/`rm` for commutative `AddReg` and non-inverting `LogicalReg`),
/// deleting the shift instruction.
fn peephole_fuse_shift_into_alu(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    zero_hi: &ZeroUpper32<'_>,
    live_out_mask: Option<u32>,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let op_i = op_at(func, i);
        let Some(sh) = decode_const_shift(op_i) else {
            continue;
        };
        let tmp = sh.dst;
        let x = sh.src;
        if !is_trackable_reg(tmp) || x == MReg::SP {
            continue;
        }
        // In SSA mode, a 32-bit `LSR` (`lsr wTmp, wX, #s`) of a `VReg` whose upper 32 bits are
        // already proven zero produces the exact same 64-bit value as `lsr xTmp, xX, #s`, so it
        // can fuse directly into a 64-bit `AddReg`/`SubReg`/`LogicalReg`.
        let narrow_lsr_as_wide = func.is_ssa
            && !sh.wide
            && x.is_virtual()
            && matches!(sh.shift, Some(MShift::Lsr(_)))
            && zero_hi.is_zero_hi(x);

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j) {
                let width_ok = |w: bool| w == sh.wide || (w && narrow_lsr_as_wide);
                let fused_candidate: Option<(MReg, MOp)> = match op_j {
                    MOp::AddReg { dst, rn, rm, shift: None, wide, sets_flags }
                        if width_ok(wide) && rn != rm =>
                    {
                        if rm == tmp {
                            Some((
                                dst,
                                MOp::AddReg {
                                    dst,
                                    rn,
                                    rm: x,
                                    shift: sh.shift,
                                    wide,
                                    sets_flags,
                                },
                            ))
                        } else if rn == tmp {
                            Some((
                                dst,
                                MOp::AddReg {
                                    dst,
                                    rn: rm,
                                    rm: x,
                                    shift: sh.shift,
                                    wide,
                                    sets_flags,
                                },
                            ))
                        } else {
                            None
                        }
                    }
                    MOp::SubReg { dst, rn, rm, shift: None, wide, sets_flags }
                        if width_ok(wide) && rn != rm && rm == tmp =>
                    {
                        Some((
                            dst,
                            MOp::SubReg {
                                dst,
                                rn,
                                rm: x,
                                shift: sh.shift,
                                wide,
                                sets_flags,
                            },
                        ))
                    }
                    MOp::LogicalReg {
                        dst,
                        rn,
                        rm,
                        shift: None,
                        invert_rm,
                        kind,
                        wide,
                    } if width_ok(wide) && rn != rm => {
                        if rm == tmp {
                            Some((
                                dst,
                                MOp::LogicalReg {
                                    dst,
                                    rn,
                                    rm: x,
                                    shift: sh.shift,
                                    invert_rm,
                                    kind,
                                    wide,
                                },
                            ))
                        } else if !invert_rm && rn == tmp {
                            Some((
                                dst,
                                MOp::LogicalReg {
                                    dst,
                                    rn: rm,
                                    rm: x,
                                    shift: sh.shift,
                                    invert_rm,
                                    kind,
                                    wide,
                                },
                            ))
                        } else {
                            None
                        }
                    }
                    MOp::Mvn { dst, rm, shift: None, wide }
                        if wide == sh.wide && rm == tmp =>
                    {
                        Some((
                            dst,
                            MOp::Mvn {
                                dst,
                                rm: x,
                                shift: sh.shift,
                                wide,
                            },
                        ))
                    }
                    _ => None,
                };

                if let Some((consumer_dst, fused)) = fused_candidate {
                    let tmp_dead = consumer_dst == tmp
                        || if func.is_ssa {
                            if let MReg::V(v) = tmp {
                                vreg_uses.get(to_idx(v.0)).copied().unwrap_or(0) == 1
                            } else {
                                is_reg_dead_after(func, end, j, tmp, vreg_uses)
                            }
                        } else {
                            is_gpr_dead_after_with_liveout(func, end, j, tmp, live_out_mask)
                        };
                    if tmp_dead {
                        dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                        dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                        mark_deleted(func, i);
                        set(&mut func.ops, j, fused);
                        inc_op_vreg_uses(&fused, &func.copy_pairs, vreg_uses);
                        stats.peepholes = stats.peepholes.saturating_add(1);
                    }
                    break;
                }
            }

            if is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_touches_reg(func, j, tmp)
                || inst_defines_reg(func, j, x)
            {
                break;
            }
        }
    }
}

/// Pass 1h-2: Fuses single-use unsigned zero-extend (`uxtb`/`uxth`/`uxtw` via `Ubfm` or
/// `LogicalImm::And #0xff/#0xffff`) directly into a following 64-bit `AddReg`/`SubReg`
/// (`shift: None`), producing `AddExt`/`SubExt` (`add/sub xD, xN, wM, uxtb/uxth/uxtw`) and
/// deleting the extend instruction.
fn peephole_fuse_uxt_into_addsub(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    live_out_mask: Option<u32>,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let op_i = op_at(func, i);
        let (tmp, x, ext) = match op_i {
            MOp::Ubfm { dst, rn, immr: 0, imms: 7, wide: false } => (dst, rn, MExt::Uxtb),
            MOp::Ubfm { dst, rn, immr: 0, imms: 15, wide: false } => (dst, rn, MExt::Uxth),
            MOp::Ubfm { dst, rn, immr: 0, imms: 31, wide: false } => (dst, rn, MExt::Uxtw),
            MOp::LogicalImm { dst, rn, imm: 0xff, kind: LogicalKind::And, .. } => {
                (dst, rn, MExt::Uxtb)
            }
            MOp::LogicalImm { dst, rn, imm: 0xffff, kind: LogicalKind::And, .. } => {
                (dst, rn, MExt::Uxth)
            }
            _ => continue,
        };
        if !is_trackable_reg(tmp) || x == MReg::SP {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j) {
                let fused_candidate: Option<(MReg, MOp)> = match op_j {
                    MOp::AddReg { dst, rn, rm, shift: None, wide: true, sets_flags }
                        if rn != rm && (sets_flags || dst != MReg::XZR) =>
                    {
                        if rm == tmp && rn != MReg::XZR {
                            Some((
                                dst,
                                MOp::AddExt { dst, rn, rm: x, ext, wide: true, sets_flags },
                            ))
                        } else if rn == tmp && rm != MReg::XZR {
                            Some((
                                dst,
                                MOp::AddExt { dst, rn: rm, rm: x, ext, wide: true, sets_flags },
                            ))
                        } else {
                            None
                        }
                    }
                    MOp::SubReg { dst, rn, rm, shift: None, wide: true, sets_flags }
                        if rn != rm
                            && rm == tmp
                            && rn != MReg::XZR
                            && (sets_flags || dst != MReg::XZR) =>
                    {
                        Some((
                            dst,
                            MOp::SubExt { dst, rn, rm: x, ext, wide: true, sets_flags },
                        ))
                    }
                    _ => None,
                };

                if let Some((consumer_dst, fused)) = fused_candidate {
                    let tmp_dead = consumer_dst == tmp
                        || if func.is_ssa {
                            if let MReg::V(v) = tmp {
                                vreg_uses.get(to_idx(v.0)).copied().unwrap_or(0) == 1
                            } else {
                                is_reg_dead_after(func, end, j, tmp, vreg_uses)
                            }
                        } else {
                            is_gpr_dead_after_with_liveout(func, end, j, tmp, live_out_mask)
                        };
                    if tmp_dead {
                        dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                        dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                        mark_deleted(func, i);
                        set(&mut func.ops, j, fused);
                        inc_op_vreg_uses(&fused, &func.copy_pairs, vreg_uses);
                        stats.peepholes = stats.peepholes.saturating_add(1);
                    }
                    break;
                }
            }

            if is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_touches_reg(func, j, tmp)
                || inst_defines_reg(func, j, x)
            {
                break;
            }
        }
    }
}

/// SSA peephole: fuses single-use `SubReg { dst: V(t), rn: Xzr, rm: y, shift, wide, sets_flags: false }`
/// (`neg t, y[, shift]`) + `AddReg { dst, rn: a, rm: b, shift: None, wide: w2, sets_flags }`
/// into `SubReg { dst, rn: other, rm: y, shift, wide, sets_flags }` and deletes the `neg`.
/// When `sets_flags == true`, only fuses if all downstream flag consumers in the block read only
/// `N`/`Z` (since `adds x, (-y)` and `subs x, y` differ on `C` when `y == 0` and on `V` when
/// `y == INT_MIN`).
fn peephole_fuse_neg_add(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    if !func.is_ssa {
        return;
    }
    for i in start..end {
        if is_protected(func, i) || vflag_def_at(func, i) != NO_VFLAG || vflag_use_at(func, i) != NO_VFLAG
        {
            continue;
        }
        let op_i = op_at(func, i);
        let MOp::SubReg {
            dst: MReg::V(t),
            rn: MReg::P(PReg::Xzr),
            rm: y,
            shift,
            wide,
            sets_flags: false,
        } = op_i
        else {
            continue;
        };
        let tmp = MReg::V(t);
        if y == MReg::SP || y == tmp {
            continue;
        }
        if vreg_uses.get(to_idx(t.0)).copied().unwrap_or(0) != 1 {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j)
                && let MOp::AddReg { dst, rn: a, rm: b, shift: None, wide: w2, sets_flags } = op_j
                && w2 == wide
                && a != b
            {
                let other_opt = if b == tmp {
                    Some(a)
                } else if a == tmp {
                    Some(b)
                } else {
                    None
                };
                if let Some(other) = other_opt {
                    let flags_ok =
                        !sets_flags || flags_consumers_satisfy(func, j, end, cond_only_nz);
                    if flags_ok {
                        let fused = MOp::SubReg {
                            dst,
                            rn: other,
                            rm: y,
                            shift,
                            wide,
                            sets_flags,
                        };
                        dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                        dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                        mark_deleted(func, i);
                        set(&mut func.ops, j, fused);
                        inc_op_vreg_uses(&fused, &func.copy_pairs, vreg_uses);
                        stats.peepholes = stats.peepholes.saturating_add(1);
                    }
                    break;
                }
            }

            if is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_touches_reg(func, j, tmp)
                || inst_defines_reg(func, j, y)
            {
                break;
            }
        }
    }
}

/// SSA peephole: fuses single-use `Csinc { dst: V(c), rn: Xzr, rm: Xzr, cond, wide }` (`cset`) +
/// later same-block `AddReg { dst, rn: a, rm: b, shift: None, wide: w2, sets_flags: false }` into
/// `Csinc { dst, rn: other, rm: other, cond, wide: w2 }` (`cinc`) when `vf = vflag_use[i]`
/// remains live and unmodified through `j`, deleting the `cset`.
/// Sound for both 32-bit and 64-bit `cset` widths because `csinc zr, zr` always yields `0` or `1`
/// across all 64 bits.
fn peephole_fuse_cset_add(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    if !func.is_ssa {
        return;
    }
    for i in start..end {
        if is_protected(func, i) || vflag_def_at(func, i) != NO_VFLAG {
            continue;
        }
        let op_i = op_at(func, i);
        let MOp::Csinc {
            dst: MReg::V(c),
            rn: MReg::P(PReg::Xzr),
            rm: MReg::P(PReg::Xzr),
            cond,
            wide: _,
        } = op_i
        else {
            continue;
        };
        if vreg_uses.get(to_idx(c.0)).copied().unwrap_or(0) != 1 {
            continue;
        }
        let vf = vflag_use_at(func, i);
        let tmp = MReg::V(c);

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j)
                && vflag_def_at(func, j) == NO_VFLAG
                && vflag_use_at(func, j) == NO_VFLAG
                && let MOp::AddReg { dst, rn: a, rm: b, shift: None, wide: w2, sets_flags: false } =
                    op_j
                && a != b
            {
                let other_opt = if b == tmp {
                    Some(a)
                } else if a == tmp {
                    Some(b)
                } else {
                    None
                };
                if let Some(other) = other_opt {
                    let fused = MOp::Csinc { dst, rn: other, rm: other, cond, wide: w2 };
                    dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                    dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                    mark_deleted(func, i);
                    set(&mut func.ops, j, fused);
                    set(&mut func.vflag_use, j, vf);
                    inc_op_vreg_uses(&fused, &func.copy_pairs, vreg_uses);
                    stats.peepholes = stats.peepholes.saturating_add(1);
                    break;
                }
            }

            if op_j.writes_flags()
                || vflag_def_at(func, j) != NO_VFLAG
                || is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_touches_reg(func, j, tmp)
            {
                break;
            }
        }
    }
}

/// Returns `true` iff `func` contains indirect jumps (`AdrTable`/`Br`) or jump-table/unwind notes
/// that introduce implicit CFG edges not captured by `func.block_succs`/`block_preds`.
fn has_indirect_or_unwind_cfg(func: &MFunc<'_>) -> bool {
    func.ops.iter().any(|op| matches!(*op, MOp::AdrTable { .. } | MOp::Br { .. }))
        || func.notes.iter().any(|note| {
            matches!(note.kind, MNoteKind::JumpTableAnchor { .. } | MNoteKind::Unwind { .. })
        })
}

#[inline]
fn gpr_bit(reg: MReg) -> u32 {
    match reg {
        MReg::P(PReg::Gpr(r)) if r <= 30 => 1u32 << r,
        _ => 0,
    }
}

#[inline]
fn opt_gpr_bit(reg: Option<MReg>) -> u32 {
    match reg {
        Some(r) => gpr_bit(r),
        None => 0,
    }
}

/// Exhaustive O(1) GPR (`x0..=x30`) `(def_mask, use_mask)` for a post-RA instruction, returning
/// `None` for calls, indirect branches, `Ret`, `ParallelCopy`, and `RawWord` which the liveness
/// solver handles explicitly.
fn op_gpr_def_use_masks(op: &MOp) -> Option<(u32, u32)> {
    Some(match *op {
        MOp::Mov { dst, src, .. } | MOp::MovSp { dst, src } => (gpr_bit(dst), gpr_bit(src)),
        MOp::MovImm { dst, .. } | MOp::Adrp { dst } | MOp::AdrTable { dst, .. } => {
            (gpr_bit(dst), 0)
        }
        MOp::AddImm { dst, rn, .. }
        | MOp::SubImm { dst, rn, .. }
        | MOp::LogicalImm { dst, rn, .. }
        | MOp::Ubfm { dst, rn, .. }
        | MOp::Sbfm { dst, rn, .. }
        | MOp::Rbit { dst, rn, .. }
        | MOp::Clz { dst, rn, .. }
        | MOp::Rev { dst, rn, .. }
        | MOp::AddLo12 { dst, rn, .. } => (gpr_bit(dst), gpr_bit(rn)),
        MOp::Mvn { dst, rm, .. } => (gpr_bit(dst), gpr_bit(rm)),
        MOp::Bfxil { dst, rn, .. } => (gpr_bit(dst), gpr_bit(dst) | gpr_bit(rn)),
        MOp::AddReg { dst, rn, rm, .. }
        | MOp::SubReg { dst, rn, rm, .. }
        | MOp::AddExt { dst, rn, rm, .. }
        | MOp::SubExt { dst, rn, rm, .. }
        | MOp::LogicalReg { dst, rn, rm, .. }
        | MOp::ShiftVar { dst, rn, rm, .. }
        | MOp::Extr { dst, rn, rm, .. }
        | MOp::Mul { dst, rn, rm, .. }
        | MOp::Umulh { dst, rn, rm, .. }
        | MOp::Smulh { dst, rn, rm, .. }
        | MOp::CarryAddSub { dst, rn, rm, .. }
        | MOp::Udiv { dst, rn, rm, .. }
        | MOp::Sdiv { dst, rn, rm, .. }
        | MOp::Umin { dst, rn, rm, .. }
        | MOp::Umax { dst, rn, rm, .. }
        | MOp::Smin { dst, rn, rm, .. }
        | MOp::Smax { dst, rn, rm, .. }
        | MOp::Csel { dst, rn, rm, .. }
        | MOp::Csinc { dst, rn, rm, .. }
        | MOp::Csinv { dst, rn, rm, .. }
        | MOp::Csneg { dst, rn, rm, .. } => (gpr_bit(dst), gpr_bit(rn) | gpr_bit(rm)),
        MOp::Madd { dst, rn, rm, ra, .. } | MOp::Msub { dst, rn, rm, ra, .. } => {
            (gpr_bit(dst), gpr_bit(rn) | gpr_bit(rm) | gpr_bit(ra))
        }
        MOp::CmpImm { rn, .. } | MOp::TstImm { rn, .. } | MOp::CcmpImm { rn, .. } => {
            (0, gpr_bit(rn))
        }
        MOp::CmpReg { rn, rm, .. } | MOp::TstReg { rn, rm, .. } | MOp::CcmpReg { rn, rm, .. } => {
            (0, gpr_bit(rn) | gpr_bit(rm))
        }
        MOp::FpAlu { dst, rn, rm, ra, .. } => {
            (gpr_bit(dst), gpr_bit(rn) | opt_gpr_bit(rm) | opt_gpr_bit(ra))
        }
        MOp::VecAlu { op: vop, dst, rn, rm, .. } => {
            let extra_dst_use = matches!(
                vop,
                VecAluOp::Bsl
                    | VecAluOp::Bit
                    | VecAluOp::Bif
                    | VecAluOp::Mla
                    | VecAluOp::Mls
                    | VecAluOp::Fmla
                    | VecAluOp::Fmls
            );
            let uses = if matches!(vop, VecAluOp::Zero) {
                0
            } else {
                (if extra_dst_use { gpr_bit(dst) } else { 0 }) | gpr_bit(rn) | opt_gpr_bit(rm)
            };
            (gpr_bit(dst), uses)
        }
        MOp::Ldr { dst, addr, .. } => match addr {
            AddrMode::BaseOff { base, .. } => (gpr_bit(dst), gpr_bit(base)),
            AddrMode::ScaledIdx { base, idx, .. } => (gpr_bit(dst), gpr_bit(base) | gpr_bit(idx)),
            AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => {
                (gpr_bit(dst) | gpr_bit(base), gpr_bit(base))
            }
        },
        MOp::Str { src, addr, .. } => match addr {
            AddrMode::BaseOff { base, .. } => (0, gpr_bit(src) | gpr_bit(base)),
            AddrMode::ScaledIdx { base, idx, .. } => {
                (0, gpr_bit(src) | gpr_bit(base) | gpr_bit(idx))
            }
            AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => {
                (gpr_bit(base), gpr_bit(src) | gpr_bit(base))
            }
        },
        MOp::Ldp { dst0, dst1, base, post_idx, pre_idx, .. } => (
            gpr_bit(dst0) | gpr_bit(dst1) | if post_idx || pre_idx { gpr_bit(base) } else { 0 },
            gpr_bit(base),
        ),
        MOp::Stp { src0, src1, base, post_idx, pre_idx, .. } => (
            if post_idx || pre_idx { gpr_bit(base) } else { 0 },
            gpr_bit(src0) | gpr_bit(src1) | gpr_bit(base),
        ),
        MOp::Prfm { base, .. } => (0, gpr_bit(base)),
        MOp::Cbz { rt, .. } | MOp::Cbnz { rt, .. } | MOp::Tbz { rt, .. } | MOp::Tbnz { rt, .. } => {
            (0, gpr_bit(rt))
        }
        MOp::FpCmp { .. }
        | MOp::Fcsel { .. }
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop => (0, 0),
        MOp::Bl
        | MOp::Blr { .. }
        | MOp::Br { .. }
        | MOp::Ret
        | MOp::ParallelCopy { .. }
        | MOp::RawWord { .. } => return None,
    })
}

/// Computes exact post-RA `live_out` 31-GPR bitmasks (`x0..=x30`) for every block in `func`,
/// returning `None` if `func.is_ssa`, if indirect/unwind CFG edges exist, or if the backward
/// fixed point does not converge within `MAX_LIVENESS_ITERS`.
fn compute_post_ra_live_out_gprs<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
) -> Option<Vec<u32, Alloc<'a>>> {
    if func.is_ssa || has_indirect_or_unwind_cfg(func) {
        return None;
    }
    let num_blocks = func.block_insts.len();
    if num_blocks == 0 {
        return None;
    }
    if func.block_succs_off.len() != num_blocks + 1 {
        func.rebuild_cfg(alloc);
    }

    let mut live_in: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    let mut live_out: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        live_in.push(0);
        live_out.push(0);
    }

    let ret_mask = (func.ret_live_gprs & ALL_GPRS_MASK) | (1u32 << 30);
    let n_ord = func.block_order.len();
    let mut converged = false;

    for _ in 0..MAX_LIVENESS_ITERS {
        let mut changed = false;
        for oi in (0..n_ord).rev() {
            let Some(&blk) = func.block_order.get(oi) else {
                continue;
            };
            let b = to_idx(blk.0);
            let mut out_m: u32 = 0;
            for &succ in func.block_succs(blk) {
                out_m |= at(&live_in, to_idx(succ.0), 0);
            }
            if out_m != at(&live_out, b, 0) {
                set(&mut live_out, b, out_m);
                changed = true;
            }

            let (start, end) = block_range(func, blk);
            let mut live = out_m;
            for idx in (start..end).rev() {
                let op = op_at(func, idx);
                match op {
                    MOp::Nop => {}
                    MOp::Ret => {
                        live = ret_mask;
                    }
                    MOp::B { target } => {
                        live = at(&live_in, to_idx(target.0), ALL_GPRS_MASK);
                    }
                    MOp::Bl
                    | MOp::Blr { .. }
                    | MOp::Br { .. }
                    | MOp::ParallelCopy { .. }
                    | MOp::RawWord { .. } => {
                        live = ALL_GPRS_MASK;
                    }
                    _ => {
                        if let Some(target) = op.branch_target() {
                            live |= at(&live_in, to_idx(target.0), ALL_GPRS_MASK);
                        }
                        if let Some((defs, uses)) = op_gpr_def_use_masks(&op) {
                            live = (live & !defs) | uses;
                        } else {
                            live = ALL_GPRS_MASK;
                        }
                    }
                }
            }
            if live != at(&live_in, b, 0) {
                set(&mut live_in, b, live);
                changed = true;
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }

    if converged { Some(live_out) } else { None }
}

/// Returns `true` iff physical GPR `reg` (`x0..=x30`) is provably dead immediately after
/// instruction `after_idx` in the block `[_, end)`, using exact block `live_out_mask` when
/// available (properly guarding mid-block conditional branches).
fn is_gpr_dead_after_with_liveout(
    func: &MFunc<'_>,
    end: usize,
    after_idx: usize,
    reg: MReg,
    live_out_mask: Option<u32>,
) -> bool {
    let bit = gpr_bit(reg);
    if bit == 0 {
        return false;
    }
    let live_out = live_out_mask.is_some_and(|m| (m & bit) == 0);
    for k in (after_idx + 1)..end {
        let op_k = op_at(func, k);
        if matches!(op_k, MOp::Nop) {
            continue;
        }
        if inst_uses_reg(func, k, reg) {
            return false;
        }
        if matches!(op_k, MOp::Ret) {
            return true;
        }
        if matches!(op_k, MOp::B { .. }) {
            return live_out;
        }
        if op_k.is_conditional_branch() {
            if !live_out {
                return false;
            }
            continue;
        }
        if matches!(
            op_k,
            MOp::Bl
                | MOp::Blr { .. }
                | MOp::Br { .. }
                | MOp::Brk { .. }
                | MOp::Udf
                | MOp::ParallelCopy { .. }
                | MOp::RawWord { .. }
        ) {
            return false;
        }
        if inst_defines_reg(func, k, reg) {
            return true;
        }
    }
    live_out
}

/// Pass 1j: Fuses `CmpImm { rn, imm12: 0, shift12: false, wide }` + sole consumer
/// `BCond { cond: Eq | Ls | Ne | Hi, target }` into `Cbz` / `Cbnz`, eliminating the `cmp`
/// instruction, freeing `NZCV`, and unlocking Pass 5 `add/sub + cbz` macro-op fusion.
///
/// Soundness for `Ls`/`Hi`: `cmp rn, #0` computes `rn - 0`, which NEVER borrows (`C == 1`),
/// so unsigned `Ls` (`C == 0 || Z == 1`) holds iff `rn == 0` and unsigned `Hi` (`C == 1 && Z == 0`)
/// holds iff `rn != 0`.
fn peephole_fuse_cmp_zero_branch(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let op_i = op_at(func, i);
        let MOp::CmpImm { rn, imm12: 0, shift12: false, wide } = op_i else {
            continue;
        };
        // `Cbz`/`Cbnz` operand role is `GprOrZr` (never `SP`).
        if !is_trackable_reg(rn) || rn == MReg::SP {
            continue;
        }
        let vf = vflag_def_at(func, i);
        if vf == NO_VFLAG || vflag_use_at(func, i) != NO_VFLAG {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        let mut found_j: Option<(usize, MCond, MBlock)> = None;
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if vflag_use_at(func, j) == vf {
                if !is_protected(func, j)
                    && let MOp::BCond { cond, target } = op_j
                    && matches!(cond, MCond::Eq | MCond::Ls | MCond::Ne | MCond::Hi)
                {
                    found_j = Some((j, cond, target));
                }
                break;
            }
            if op_j.reads_flags()
                || op_j.writes_flags()
                || vflag_def_at(func, j) != NO_VFLAG
                || is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_defines_reg(func, j, rn)
            {
                break;
            }
        }

        let Some((j, cond, target)) = found_j else {
            continue;
        };

        // Ensure no subsequent instruction in the block consumes `vf` or reads `NZCV` before a
        // fresh flag write.
        let mut flag_used_after = false;
        for k in (j + 1)..end {
            let op_k = op_at(func, k);
            if matches!(op_k, MOp::Nop) {
                continue;
            }
            if vflag_use_at(func, k) == vf || op_k.reads_flags() {
                flag_used_after = true;
                break;
            }
            if op_k.writes_flags() || vflag_def_at(func, k) != NO_VFLAG {
                break;
            }
        }
        if flag_used_after {
            continue;
        }

        let new_br = match cond {
            MCond::Eq | MCond::Ls => MOp::Cbz { rt: rn, target, wide },
            MCond::Ne | MCond::Hi => MOp::Cbnz { rt: rn, target, wide },
            _ => continue,
        };
        let op_j = op_at(func, j);
        dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
        dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
        mark_deleted(func, i);
        set(&mut func.ops, j, new_br);
        set(&mut func.vflag_use, j, NO_VFLAG);
        inc_op_vreg_uses(&new_br, &func.copy_pairs, vreg_uses);
        stats.peepholes = stats.peepholes.saturating_add(1);
    }
}

/// Pass 1k: Folds `MovImm { dst: k, imm: 0 | 1 }` feeding `Csel` into `Csel(..., Xzr)` (for `0`)
/// or `Csinc(..., Xzr)` (for `1`), deleting `MovImm` when `k` becomes provably dead.
fn peephole_fold_const_csel(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    live_out_mask: Option<u32>,
    vreg_uses: &mut [u32],
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let op_i = op_at(func, i);
        let MOp::MovImm { dst: k, imm, .. } = op_i else {
            continue;
        };
        if (imm != 0 && imm != 1) || !is_trackable_reg(k) || k == MReg::SP {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        let mut rewrote_any = false;
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if !is_protected(func, j)
                && let MOp::Csel { dst, rn, rm, cond, wide } = op_j
                && !matches!(cond, MCond::Al | MCond::Nv)
                && ((rn == k) ^ (rm == k))
            {
                let replacement = if imm == 0 {
                    if rn == k {
                        MOp::Csel { dst, rn: MReg::XZR, rm, cond, wide }
                    } else {
                        MOp::Csel { dst, rn, rm: MReg::XZR, cond, wide }
                    }
                } else if rn == k {
                    MOp::Csinc { dst, rn: rm, rm: MReg::XZR, cond: cond.invert(), wide }
                } else {
                    MOp::Csinc { dst, rn, rm: MReg::XZR, cond, wide }
                };
                dec_op_vreg_uses(&op_j, &func.copy_pairs, vreg_uses);
                set(&mut func.ops, j, replacement);
                inc_op_vreg_uses(&replacement, &func.copy_pairs, vreg_uses);
                stats.peepholes = stats.peepholes.saturating_add(1);
                rewrote_any = true;
            }
            if inst_defines_reg(func, j, k)
                || is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
            {
                break;
            }
        }

        if rewrote_any {
            let dead = if func.is_ssa {
                if let MReg::V(v) = k {
                    vreg_uses.get(to_idx(v.0)).copied().unwrap_or(1) == 0
                } else {
                    false
                }
            } else {
                is_gpr_dead_after_with_liveout(func, end, i, k, live_out_mask)
            };
            if dead {
                dec_op_vreg_uses(&op_i, &func.copy_pairs, vreg_uses);
                mark_deleted(func, i);
                stats.peepholes = stats.peepholes.saturating_add(1);
            }
        }
    }
}

/// Pass 1l-1 (post-RA only): Propagates 1-word `MovImm` constants through lowered `ParallelCopy`
/// `Mov { dst, src, wide: true }` chains (e.g. `mov x2, #1; mov x16, x2; mov x0, x16` ->
/// `mov x0, #1`) and sweeps any newly dead `MovImm`/`Mov` instructions using exact `live_out_mask`.
fn peephole_post_ra_const_copy_prop(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    live_out_mask: Option<u32>,
    stats: &mut MOptStats,
) {
    let mut table = [ImmCseSlot::EMPTY; IMM_CSE_SLOTS];
    let mut next_evict: usize = 0;
    let mut rewrote_any = false;

    for i in start..end {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }
        if let MOp::MovImm { dst, imm, wide } = op {
            let imm_norm = if wide { imm } else { imm & 0xffff_ffff };
            for slot in &mut table {
                if slot.valid && slot.reg == dst {
                    slot.valid = false;
                }
            }
            if gpr_bit(dst) != 0 && mov_imm_inst_cost(imm_norm, wide) == 1 {
                let mut inserted = false;
                for slot in &mut table {
                    if !slot.valid {
                        *slot = ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide };
                        inserted = true;
                        break;
                    }
                }
                if !inserted {
                    set(
                        &mut table,
                        next_evict % IMM_CSE_SLOTS,
                        ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide },
                    );
                    next_evict = next_evict.wrapping_add(1);
                }
            }
            continue;
        }
        if !is_protected(func, i)
            && let MOp::Mov { dst, src, wide: true } = op
            && dst != src
            && gpr_bit(dst) != 0
            && gpr_bit(src) != 0
        {
            let mut found: Option<(u64, bool)> = None;
            for slot in &table {
                if slot.valid && slot.reg == src {
                    found = Some((slot.imm, slot.wide));
                    break;
                }
            }
            if let Some((imm_norm, src_wide)) = found {
                let new_op = MOp::MovImm { dst, imm: imm_norm, wide: src_wide };
                set(&mut func.ops, i, new_op);
                set(&mut func.flags, i, MInstFlags::NONE);
                stats.peepholes = stats.peepholes.saturating_add(1);
                rewrote_any = true;
                for slot in &mut table {
                    if slot.valid && slot.reg == dst {
                        slot.valid = false;
                    }
                }
                let mut inserted = false;
                for slot in &mut table {
                    if !slot.valid {
                        *slot = ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide: src_wide };
                        inserted = true;
                        break;
                    }
                }
                if !inserted {
                    set(
                        &mut table,
                        next_evict % IMM_CSE_SLOTS,
                        ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide: src_wide },
                    );
                    next_evict = next_evict.wrapping_add(1);
                }
                continue;
            }
        }
        if is_call_or_mem_barrier(func, i) || is_control_or_call(&op) {
            for slot in &mut table {
                slot.valid = false;
            }
            continue;
        }
        for slot in &mut table {
            if slot.valid && inst_defines_reg(func, i, slot.reg) {
                slot.valid = false;
            }
        }
    }

    if rewrote_any {
        for i in (start..end).rev() {
            if is_protected(func, i) {
                continue;
            }
            let op = op_at(func, i);
            let dead_dst = match op {
                MOp::MovImm { dst, .. } | MOp::Mov { dst, .. } if gpr_bit(dst) != 0 => Some(dst),
                _ => None,
            };
            if let Some(dst) = dead_dst
                && is_gpr_dead_after_with_liveout(func, end, i, dst, live_out_mask)
            {
                mark_deleted(func, i);
                stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
            }
        }
    }
}

/// Returns `Some((old_dst, rewritten_op))` when `op` is a pure single-GPR-destination instruction
/// whose destination can legally be rewritten directly to physical GPR `new_dst` (`x0..=x30`),
/// excluding read-modify-write (`Bfxil`), writeback loads (`PostIdx`/`PreIdx`), `Ldp`, `MovSp`,
/// FP/vector loads, calls, and opaque instructions.
fn rewrite_pure_single_gpr_dst(op: MOp, new_dst: MReg) -> Option<(MReg, MOp)> {
    if gpr_bit(new_dst) == 0 {
        return None;
    }
    let out = match op {
        MOp::MovImm { dst, imm, wide } => (dst, MOp::MovImm { dst: new_dst, imm, wide }),
        MOp::AddImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            (dst, MOp::AddImm { dst: new_dst, rn, imm12, shift12, wide, sets_flags })
        }
        MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            (dst, MOp::SubImm { dst: new_dst, rn, imm12, shift12, wide, sets_flags })
        }
        MOp::AddReg { dst, rn, rm, shift, wide, sets_flags } => {
            (dst, MOp::AddReg { dst: new_dst, rn, rm, shift, wide, sets_flags })
        }
        MOp::SubReg { dst, rn, rm, shift, wide, sets_flags } => {
            (dst, MOp::SubReg { dst: new_dst, rn, rm, shift, wide, sets_flags })
        }
        MOp::AddExt { dst, rn, rm, ext, wide, sets_flags } => {
            (dst, MOp::AddExt { dst: new_dst, rn, rm, ext, wide, sets_flags })
        }
        MOp::SubExt { dst, rn, rm, ext, wide, sets_flags } => {
            (dst, MOp::SubExt { dst: new_dst, rn, rm, ext, wide, sets_flags })
        }
        MOp::LogicalImm { dst, rn, imm, kind, wide } => {
            (dst, MOp::LogicalImm { dst: new_dst, rn, imm, kind, wide })
        }
        MOp::LogicalReg { dst, rn, rm, shift, kind, invert_rm, wide } => {
            (dst, MOp::LogicalReg { dst: new_dst, rn, rm, shift, kind, invert_rm, wide })
        }
        MOp::Mvn { dst, rm, shift, wide } => (dst, MOp::Mvn { dst: new_dst, rm, shift, wide }),
        MOp::ShiftVar { dst, rn, rm, kind, wide } => {
            (dst, MOp::ShiftVar { dst: new_dst, rn, rm, kind, wide })
        }
        MOp::Ubfm { dst, rn, immr, imms, wide } => {
            (dst, MOp::Ubfm { dst: new_dst, rn, immr, imms, wide })
        }
        MOp::Sbfm { dst, rn, immr, imms, wide } => {
            (dst, MOp::Sbfm { dst: new_dst, rn, immr, imms, wide })
        }
        MOp::Extr { dst, rn, rm, lsb, wide } => {
            (dst, MOp::Extr { dst: new_dst, rn, rm, lsb, wide })
        }
        MOp::Rbit { dst, rn, wide } => (dst, MOp::Rbit { dst: new_dst, rn, wide }),
        MOp::Clz { dst, rn, wide } => (dst, MOp::Clz { dst: new_dst, rn, wide }),
        MOp::Rev { dst, rn, wide } => (dst, MOp::Rev { dst: new_dst, rn, wide }),
        MOp::Mul { dst, rn, rm, wide } => (dst, MOp::Mul { dst: new_dst, rn, rm, wide }),
        MOp::Madd { dst, rn, rm, ra, wide } => (dst, MOp::Madd { dst: new_dst, rn, rm, ra, wide }),
        MOp::Msub { dst, rn, rm, ra, wide } => (dst, MOp::Msub { dst: new_dst, rn, rm, ra, wide }),
        MOp::Umulh { dst, rn, rm } => (dst, MOp::Umulh { dst: new_dst, rn, rm }),
        MOp::Smulh { dst, rn, rm } => (dst, MOp::Smulh { dst: new_dst, rn, rm }),
        MOp::CarryAddSub { dst, rn, rm, is_sub, sets_flags } => {
            (dst, MOp::CarryAddSub { dst: new_dst, rn, rm, is_sub, sets_flags })
        }
        MOp::Udiv { dst, rn, rm, wide } => (dst, MOp::Udiv { dst: new_dst, rn, rm, wide }),
        MOp::Sdiv { dst, rn, rm, wide } => (dst, MOp::Sdiv { dst: new_dst, rn, rm, wide }),
        MOp::Umin { dst, rn, rm, wide } => (dst, MOp::Umin { dst: new_dst, rn, rm, wide }),
        MOp::Umax { dst, rn, rm, wide } => (dst, MOp::Umax { dst: new_dst, rn, rm, wide }),
        MOp::Smin { dst, rn, rm, wide } => (dst, MOp::Smin { dst: new_dst, rn, rm, wide }),
        MOp::Smax { dst, rn, rm, wide } => (dst, MOp::Smax { dst: new_dst, rn, rm, wide }),
        MOp::Csel { dst, rn, rm, cond, wide } => {
            (dst, MOp::Csel { dst: new_dst, rn, rm, cond, wide })
        }
        MOp::Csinc { dst, rn, rm, cond, wide } => {
            (dst, MOp::Csinc { dst: new_dst, rn, rm, cond, wide })
        }
        MOp::Csinv { dst, rn, rm, cond, wide } => {
            (dst, MOp::Csinv { dst: new_dst, rn, rm, cond, wide })
        }
        MOp::Csneg { dst, rn, rm, cond, wide } => {
            (dst, MOp::Csneg { dst: new_dst, rn, rm, cond, wide })
        }
        MOp::Adrp { dst } => (dst, MOp::Adrp { dst: new_dst }),
        MOp::AddLo12 { dst, rn } => (dst, MOp::AddLo12 { dst: new_dst, rn }),
        MOp::Ldr { dst, addr, size, signed }
            if !size.is_fp_or_vec()
                && matches!(addr, AddrMode::BaseOff { .. } | AddrMode::ScaledIdx { .. }) =>
        {
            (dst, MOp::Ldr { dst: new_dst, addr, size, signed })
        }
        _ => return None,
    };
    if gpr_bit(out.0) == 0 {
        return None;
    }
    Some(out)
}

/// Pass 1l-2 (post-RA only): Backward single-def copy folding (`op dst=rA; ...; mov rB, rA` ->
/// `op dst=rB`) when `rA` and `rB` are untouched in between, no intervening trapping/control op
/// sits between them, and `rA` is provably dead after the `mov`. Runs before Pass 3 so folded
/// pointer increments (`add x6, x20, #1; mov x20, x6` -> `add x20, x20, #1`) immediately unlock
/// post-index `[x20], #1` fusion.
fn peephole_post_ra_backward_copy_fold(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    live_out_mask: Option<u32>,
    stats: &mut MOptStats,
) {
    for j in start..end {
        if is_protected(func, j) {
            continue;
        }
        let MOp::Mov { dst: r_b, src: r_a, wide: true } = op_at(func, j) else {
            continue;
        };
        if r_a == r_b || gpr_bit(r_a) == 0 || gpr_bit(r_b) == 0 {
            continue;
        }
        if !is_gpr_dead_after_with_liveout(func, end, j, r_a, live_out_mask) {
            continue;
        }

        let scan_start = j.saturating_sub(PEEPHOLE_WINDOW).max(start);
        let mut i = j;
        while i > scan_start {
            i -= 1;
            let op_i = op_at(func, i);
            if matches!(op_i, MOp::Nop) {
                continue;
            }
            if inst_defines_reg(func, i, r_a) {
                // Preserve in-place `AddImm`/`SubImm { dst == rn }` before Pass 3 so post-index
                // load/store writeback (`[base], #imm`) is never broken by retargeting `dst`.
                let is_inplace_ptr_step = matches!(
                    op_i,
                    MOp::AddImm { dst, rn, .. } | MOp::SubImm { dst, rn, .. } if dst == rn
                );
                if !is_protected(func, i)
                    && !is_inplace_ptr_step
                    && let Some((old_dst, rewritten)) = rewrite_pure_single_gpr_dst(op_i, r_b)
                    && old_dst == r_a
                {
                    set(&mut func.ops, i, rewritten);
                    mark_deleted(func, j);
                    stats.movs_eliminated = stats.movs_eliminated.saturating_add(1);
                }
                break;
            }
            if is_control_or_call(&op_i)
                || is_call_or_mem_barrier(func, i)
                || flags_at(func, i).contains(MInstFlags::MAY_TRAP)
                || matches!(op_i, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                || inst_uses_reg(func, i, r_a)
                || inst_touches_reg(func, i, r_b)
            {
                break;
            }
        }
    }
}

/// Pass 1l-3 (post-RA only): Deletes redundant `uxtb`/`uxth` (`Ubfm { immr: 0, imms: 7 | 15, wide: false }`)
/// whose sole remaining use before death is the value operand (`src`, never `addr`) of a matching
/// narrow `Str(B1)` / `Str(B2)`.
fn peephole_post_ra_uxtb_str(
    func: &mut MFunc<'_>,
    start: usize,
    end: usize,
    live_out_mask: Option<u32>,
    stats: &mut MOptStats,
) {
    for i in start..end {
        if is_protected(func, i) {
            continue;
        }
        let MOp::Ubfm { dst: r, rn, immr: 0, imms, wide: false } = op_at(func, i) else {
            continue;
        };
        let req_size = match imms {
            7 => MemSize::B8,
            15 => MemSize::H16,
            _ => continue,
        };
        if gpr_bit(r) == 0 || gpr_bit(rn) == 0 {
            continue;
        }

        let scan_end = end.min(i.saturating_add(PEEPHOLE_WINDOW));
        let mut str_idx: Option<usize> = None;
        for j in (i + 1)..scan_end {
            let op_j = op_at(func, j);
            if matches!(op_j, MOp::Nop) {
                continue;
            }
            if inst_uses_reg(func, j, r) {
                if !is_protected(func, j)
                    && let MOp::Str { src, addr, size } = op_j
                    && src == r
                    && size == req_size
                    && !addr_uses_reg(addr, r)
                    && is_gpr_dead_after_with_liveout(func, end, j, r, live_out_mask)
                {
                    let wb_conflict = match addr {
                        AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } => {
                            base == rn
                        }
                        AddrMode::BaseOff { .. } | AddrMode::ScaledIdx { .. } => false,
                    };
                    if !wb_conflict {
                        str_idx = Some(j);
                    }
                }
                break;
            }
            if inst_defines_reg(func, j, r)
                || (rn != r && inst_defines_reg(func, j, rn))
                || is_control_or_call(&op_j)
                || is_call_or_mem_barrier(func, j)
                || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
            {
                break;
            }
        }

        if let Some(j) = str_idx {
            if r != rn
                && let MOp::Str { addr, size, .. } = op_at(func, j)
            {
                set(&mut func.ops, j, MOp::Str { src: rn, addr, size });
            }
            mark_deleted(func, i);
            stats.peepholes = stats.peepholes.saturating_add(1);
        }
    }
}

/// Pass 1m (post-RA only): Cross-block known-`MovImm` elision over CFG predecessors.
///
/// Propagates exact `(GPR -> (imm_norm, wide))` bindings forward along `func.block_order`:
/// when a block's CFG predecessors are all strictly earlier in `block_order`, its entry state is
/// the intersection of its predecessors' exit states (exact copy for single-predecessor blocks).
/// Any unprotected `MovImm { dst, imm, wide }` already guaranteed live in `dst` is deleted.
fn mopt_cross_block_mov_imm_post_ra<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
    stats: &mut MOptStats,
) {
    if func.is_ssa || has_indirect_or_unwind_cfg(func) {
        return;
    }
    let num_blocks = func.block_insts.len();
    if num_blocks <= 1 {
        return;
    }
    if func.block_preds_off.len() != num_blocks + 1 {
        func.rebuild_cfg(alloc);
    }

    let mut ord_pos: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        ord_pos.push(u32::MAX);
    }
    for (pos, &blk) in func.block_order.iter().enumerate() {
        set(&mut ord_pos, to_idx(blk.0), u32_of(pos).unwrap_or(u32::MAX));
    }

    let total_slots = num_blocks.saturating_mul(CROSS_BLOCK_IMM_SLOTS);
    let mut exit_tables: Vec<ImmCseSlot, Alloc<'a>> = Vec::with_capacity_in(total_slots, alloc);
    for _ in 0..total_slots {
        exit_tables.push(ImmCseSlot::EMPTY);
    }

    let n_ord = func.block_order.len();
    for bi in 0..n_ord {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let b = to_idx(blk.0);
        let my_pos = at(&ord_pos, b, u32::MAX);
        let mut table = [ImmCseSlot::EMPTY; CROSS_BLOCK_IMM_SLOTS];

        let preds = func.block_preds(blk);
        if bi > 0 && !preds.is_empty() {
            let all_earlier = preds
                .iter()
                .all(|&p| at(&ord_pos, to_idx(p.0), u32::MAX) < my_pos);
            if all_earlier && let Some(&first_p) = preds.first() {
                let fp_base = to_idx(first_p.0).saturating_mul(CROSS_BLOCK_IMM_SLOTS);
                let mut out_k: usize = 0;
                for s in 0..CROSS_BLOCK_IMM_SLOTS {
                    let cand = at(&exit_tables, fp_base + s, ImmCseSlot::EMPTY);
                    if !cand.valid {
                        continue;
                    }
                    let mut in_all = true;
                    for &other_p in preds.iter().skip(1) {
                        let op_base = to_idx(other_p.0).saturating_mul(CROSS_BLOCK_IMM_SLOTS);
                        let mut matched = false;
                        for os in 0..CROSS_BLOCK_IMM_SLOTS {
                            let o = at(&exit_tables, op_base + os, ImmCseSlot::EMPTY);
                            if o.valid
                                && o.reg == cand.reg
                                && o.imm == cand.imm
                                && o.wide == cand.wide
                            {
                                matched = true;
                                break;
                            }
                        }
                        if !matched {
                            in_all = false;
                            break;
                        }
                    }
                    if in_all && out_k < CROSS_BLOCK_IMM_SLOTS {
                        set(&mut table, out_k, cand);
                        out_k += 1;
                    }
                }
            }
        }

        let mut exit_table = table;
        let mut seen_cond_branch = false;
        let mut next_evict: usize = 0;
        let mut exit_next_evict: usize = 0;
        let (start, end) = block_range(func, blk);
        for i in start..end {
            let op = op_at(func, i);
            if matches!(op, MOp::Nop) {
                continue;
            }
            if op.is_conditional_branch() {
                seen_cond_branch = true;
                continue;
            }
            if let MOp::MovImm { dst, imm, wide } = op {
                let imm_norm = if wide { imm } else { imm & 0xffff_ffff };
                let mut already_same = false;
                for slot in &table {
                    if slot.valid && slot.reg == dst && slot.imm == imm_norm && slot.wide == wide {
                        already_same = true;
                        break;
                    }
                }
                if already_same && !is_protected(func, i) && gpr_bit(dst) != 0 {
                    mark_deleted(func, i);
                    stats.peepholes = stats.peepholes.saturating_add(1);
                    continue;
                }
                for slot in &mut table {
                    if slot.valid && slot.reg == dst {
                        slot.valid = false;
                    }
                }
                for slot in &mut exit_table {
                    if slot.valid && slot.reg == dst {
                        slot.valid = false;
                    }
                }
                if gpr_bit(dst) != 0 {
                    let mut inserted = false;
                    for slot in &mut table {
                        if !slot.valid {
                            *slot = ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide };
                            inserted = true;
                            break;
                        }
                    }
                    if !inserted {
                        set(
                            &mut table,
                            next_evict % CROSS_BLOCK_IMM_SLOTS,
                            ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide },
                        );
                        next_evict = next_evict.wrapping_add(1);
                    }
                    // Only expose bindings defined BEFORE any mid-block conditional branch in
                    // `exit_table`, so taken branch targets never observe post-branch `MovImm`s.
                    if !seen_cond_branch {
                        let mut ex_inserted = false;
                        for slot in &mut exit_table {
                            if !slot.valid {
                                *slot = ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide };
                                ex_inserted = true;
                                break;
                            }
                        }
                        if !ex_inserted {
                            set(
                                &mut exit_table,
                                exit_next_evict % CROSS_BLOCK_IMM_SLOTS,
                                ImmCseSlot { valid: true, reg: dst, imm: imm_norm, wide },
                            );
                            exit_next_evict = exit_next_evict.wrapping_add(1);
                        }
                    }
                }
                continue;
            }
            // Note: branches do not clobber GPRs, so only calls/mem barriers/opaque ops clear all.
            if is_call_or_mem_barrier(func, i)
                || matches!(
                    op,
                    MOp::Bl
                        | MOp::Blr { .. }
                        | MOp::Br { .. }
                        | MOp::Ret
                        | MOp::ParallelCopy { .. }
                        | MOp::RawWord { .. }
                )
            {
                for slot in &mut table {
                    slot.valid = false;
                }
                for slot in &mut exit_table {
                    slot.valid = false;
                }
                continue;
            }
            for slot in &mut table {
                if slot.valid && inst_defines_reg(func, i, slot.reg) {
                    slot.valid = false;
                }
            }
            for slot in &mut exit_table {
                if slot.valid && inst_defines_reg(func, i, slot.reg) {
                    slot.valid = false;
                }
            }
        }

        let my_base = b.saturating_mul(CROSS_BLOCK_IMM_SLOTS);
        for (s, &slot) in exit_table.iter().enumerate() {
            set(&mut exit_tables, my_base + s, slot);
        }
    }
}

// -----------------------------------------------------------------------------
// Pass 2: Zero-Allocation Intra-MBB Redundant Load Elimination & Store Forwarding
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct LoadSlot {
    valid: bool,
    base: MReg,
    off: i32,
    size: MemSize,
    signed: bool,
    from_store: bool,
    val: MReg,
}

impl LoadSlot {
    const EMPTY: Self = Self {
        valid: false,
        base: MReg::XZR,
        off: 0,
        size: MemSize::X64,
        signed: false,
        from_store: false,
        val: MReg::XZR,
    };
}

#[inline]
fn byte_ranges_overlap(off_a: i32, bytes_a: u32, off_b: i32, bytes_b: u32) -> bool {
    let a0 = i64::from(off_a);
    let a1 = a0 + i64::from(bytes_a);
    let b0 = i64::from(off_b);
    let b1 = b0 + i64::from(bytes_b);
    a0 < b1 && b0 < a1
}

fn record_load_slot(
    table: &mut [LoadSlot; LOAD_SLOTS],
    next_evict: &mut usize,
    entry: LoadSlot,
) {
    // Update existing exact `(base, off, size, signed, val)` key if present, keeping multiple
    // registers simultaneously mapped to the same stack/memory slot when copied or forwarded.
    for slot in table.iter_mut() {
        if slot.valid
            && slot.base == entry.base
            && slot.off == entry.off
            && slot.size == entry.size
            && slot.signed == entry.signed
            && slot.val == entry.val
        {
            slot.from_store = slot.from_store && entry.from_store;
            return;
        }
    }
    for slot in table.iter_mut() {
        if !slot.valid {
            *slot = entry;
            return;
        }
    }
    set(table, *next_evict % LOAD_SLOTS, entry);
    *next_evict = next_evict.wrapping_add(1);
}

#[inline]
fn invalidate_all_slots(table: &mut [LoadSlot; LOAD_SLOTS]) {
    for slot in table.iter_mut() {
        slot.valid = false;
    }
}

#[inline]
fn invalidate_slots_for_store(
    table: &mut [LoadSlot; LOAD_SLOTS],
    store_base: MReg,
    store_off: i32,
    store_bytes: u32,
) {
    for slot in table.iter_mut() {
        if !slot.valid {
            continue;
        }
        // Same base with provably disjoint byte range does NOT alias; any other base may alias!
        if slot.base != store_base
            || byte_ranges_overlap(slot.off, slot.size.byte_size(), store_off, store_bytes)
        {
            slot.valid = false;
        }
    }
}

#[inline]
fn invalidate_slots_for_reg_def(table: &mut [LoadSlot; LOAD_SLOTS], def_reg: MReg) {
    if !is_trackable_reg(def_reg) {
        return;
    }
    for slot in table.iter_mut() {
        if slot.valid && (slot.base == def_reg || slot.val == def_reg) {
            slot.valid = false;
        }
    }
}

/// Pass 2: intra-MBB redundant load elimination & store-to-load forwarding (strictly O(N), zero
/// heap allocation via a fixed 32-slot stack table).
fn mopt_redundant_loads(func: &mut MFunc<'_>, stats: &mut MOptStats) {
    let n_order = func.block_order.len();
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        if start >= end {
            continue;
        }

        let mut table = [LoadSlot::EMPTY; LOAD_SLOTS];
        let mut next_evict: usize = 0;

        for i in start..end {
            let op = op_at(func, i);
            if matches!(op, MOp::Nop) {
                continue;
            }
            let volatile = is_volatile_inst(func, i);
            if volatile || is_call_or_mem_barrier(func, i) {
                invalidate_all_slots(&mut table);
                continue;
            }

            match op {
                MOp::Ldr { dst, addr: AddrMode::BaseOff { base, off }, size, signed } => {
                    // Look for the best sound forwarding match (preferring `slot.val == dst` so
                    // the redundant load deletes outright, then non-store-originated slots).
                    let mut hit: Option<LoadSlot> = None;
                    let mut hit_rank: u8 = 0;
                    for slot in &table {
                        if !slot.valid
                            || slot.base != base
                            || slot.off != off
                            || slot.size != size
                        {
                            continue;
                        }
                        let compat = if !slot.from_store {
                            slot.signed == signed
                        } else if slot.val == MReg::XZR && !size.is_fp_or_vec() {
                            // Zero-stored bytes zero/sign-extend identically to zero.
                            true
                        } else {
                            // `Str(W32)` forwards to unsigned `Ldr(W32)` via zero-extending 32-bit
                            // `Mov`; `X64`/`S32`/`D64`/`Q128` forward exact width. `B8`/`H16`
                            // stores may have dirty upper bits in `src` so cannot forward directly.
                            !signed
                                && matches!(
                                    size,
                                    MemSize::W32
                                        | MemSize::X64
                                        | MemSize::S32
                                        | MemSize::D64
                                        | MemSize::Q128
                                )
                        };
                        if compat {
                            let slot_needs_zext = slot.from_store && size == MemSize::W32;
                            let rank: u8 = if slot.val == dst && !slot_needs_zext {
                                3
                            } else if !slot.from_store {
                                2
                            } else {
                                1
                            };
                            if rank > hit_rank {
                                hit = Some(*slot);
                                hit_rank = rank;
                                if rank == 3 {
                                    break;
                                }
                            }
                        }
                    }

                    if !is_protected(func, i)
                        && let Some(slot) = hit
                    {
                        let needs_w32_zext = slot.from_store && size == MemSize::W32;
                        if dst == slot.val && !needs_w32_zext {
                            mark_deleted(func, i);
                            stats.loads_forwarded = stats.loads_forwarded.saturating_add(1);
                            continue;
                        }
                        let fwd_op = match size {
                            MemSize::B8 | MemSize::H16 | MemSize::W32 | MemSize::X64 => MOp::Mov {
                                dst,
                                src: slot.val,
                                wide: !needs_w32_zext,
                            },
                            MemSize::S32 => MOp::FpAlu {
                                op: FpAluOp::Mov,
                                dst,
                                rn: slot.val,
                                rm: None,
                                ra: None,
                                double: false,
                            },
                            MemSize::D64 => MOp::FpAlu {
                                op: FpAluOp::Mov,
                                dst,
                                rn: slot.val,
                                rm: None,
                                ra: None,
                                double: true,
                            },
                            MemSize::Q128 => MOp::VecAlu {
                                op: VecAluOp::Mov,
                                dst,
                                rn: slot.val,
                                rm: None,
                                elem_size: 0,
                            },
                        };
                        set(&mut func.ops, i, fwd_op);
                        set(&mut func.flags, i, MInstFlags::NONE);
                        stats.loads_forwarded = stats.loads_forwarded.saturating_add(1);
                        invalidate_slots_for_reg_def(&mut table, dst);
                        if dst != base && is_trackable_reg(dst) {
                            record_load_slot(
                                &mut table,
                                &mut next_evict,
                                LoadSlot {
                                    valid: true,
                                    base,
                                    off,
                                    size,
                                    signed,
                                    from_store: false,
                                    val: dst,
                                },
                            );
                        }
                        continue;
                    }

                    // Not forwarded: invalidate slots affected by writing `dst` (including
                    // pointer-chasing `dst == base`!), then record this load if `dst != base`.
                    invalidate_slots_for_reg_def(&mut table, dst);
                    if dst != base && is_trackable_reg(dst) {
                        record_load_slot(
                            &mut table,
                            &mut next_evict,
                            LoadSlot {
                                valid: true,
                                base,
                                off,
                                size,
                                signed,
                                from_store: false,
                                val: dst,
                            },
                        );
                    }
                }
                MOp::Str { src, addr: AddrMode::BaseOff { base, off }, size } => {
                    invalidate_slots_for_store(&mut table, base, off, size.byte_size());
                    let can_forward_later = matches!(
                        size,
                        MemSize::W32 | MemSize::X64 | MemSize::S32 | MemSize::D64 | MemSize::Q128
                    ) || (src == MReg::XZR && !size.is_fp_or_vec());
                    if can_forward_later {
                        record_load_slot(
                            &mut table,
                            &mut next_evict,
                            LoadSlot {
                                valid: true,
                                base,
                                off,
                                size,
                                signed: false,
                                from_store: true,
                                val: src,
                            },
                        );
                    }
                }
                MOp::Ldp { dst0, dst1, base, off, size, post_idx, pre_idx } => {
                    invalidate_slots_for_reg_def(&mut table, dst0);
                    invalidate_slots_for_reg_def(&mut table, dst1);
                    if post_idx || pre_idx {
                        invalidate_slots_for_reg_def(&mut table, base);
                    } else if dst0 != base && dst1 != base {
                        let off0 = i32::from(off);
                        let elem = i32::try_from(size.byte_size()).unwrap_or(8);
                        if is_trackable_reg(dst0) {
                            record_load_slot(
                                &mut table,
                                &mut next_evict,
                                LoadSlot {
                                    valid: true,
                                    base,
                                    off: off0,
                                    size,
                                    signed: false,
                                    from_store: false,
                                    val: dst0,
                                },
                            );
                        }
                        if is_trackable_reg(dst1) && dst1 != dst0 {
                            record_load_slot(
                                &mut table,
                                &mut next_evict,
                                LoadSlot {
                                    valid: true,
                                    base,
                                    off: off0.saturating_add(elem),
                                    size,
                                    signed: false,
                                    from_store: false,
                                    val: dst1,
                                },
                            );
                        }
                    }
                }
                MOp::Stp { src0, src1, base, off, size, post_idx, pre_idx } => {
                    if post_idx || pre_idx {
                        invalidate_all_slots(&mut table);
                    } else {
                        let off0 = i32::from(off);
                        let bytes = size.byte_size();
                        invalidate_slots_for_store(&mut table, base, off0, bytes.saturating_mul(2));
                        let elem = i32::try_from(bytes).unwrap_or(8);
                        record_load_slot(
                            &mut table,
                            &mut next_evict,
                            LoadSlot {
                                valid: true,
                                base,
                                off: off0,
                                size,
                                signed: false,
                                from_store: true,
                                val: src0,
                            },
                        );
                        record_load_slot(
                            &mut table,
                            &mut next_evict,
                            LoadSlot {
                                valid: true,
                                base,
                                off: off0.saturating_add(elem),
                                size,
                                signed: false,
                                from_store: true,
                                val: src1,
                            },
                        );
                    }
                }
                MOp::Str { .. } => {
                    // `ScaledIdx` / `PostIdx` / `PreIdx` store: conservatively invalidate all.
                    invalidate_all_slots(&mut table);
                }
                MOp::Ldr { dst, addr, .. } => {
                    invalidate_slots_for_reg_def(&mut table, dst);
                    if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                        invalidate_slots_for_reg_def(&mut table, base);
                    }
                }
                MOp::Mov { dst, src, wide: true } => {
                    if dst != src
                        && dst != MReg::SP
                        && src != MReg::SP
                        && is_trackable_reg(dst)
                        && is_trackable_reg(src)
                    {
                        let mut copied = [LoadSlot::EMPTY; LOAD_SLOTS];
                        let mut n_copied: usize = 0;
                        for slot in &table {
                            if slot.valid
                                && slot.val == src
                                && slot.base != dst
                                && !slot.size.is_fp_or_vec()
                            {
                                let mut dup = *slot;
                                dup.val = dst;
                                set(&mut copied, n_copied, dup);
                                n_copied += 1;
                            }
                        }
                        invalidate_slots_for_reg_def(&mut table, dst);
                        for dup in copied.iter().take(n_copied) {
                            record_load_slot(&mut table, &mut next_evict, *dup);
                        }
                    } else {
                        invalidate_slots_for_reg_def(&mut table, dst);
                    }
                }
                MOp::RawWord { .. } => {
                    invalidate_all_slots(&mut table);
                }
                _ => {
                    if flags_at(func, i).contains(MInstFlags::MEM_WRITE) || is_control_or_call(&op)
                    {
                        invalidate_all_slots(&mut table);
                    } else {
                        for slot in &mut table {
                            if slot.valid
                                && (inst_defines_reg(func, i, slot.base)
                                    || inst_defines_reg(func, i, slot.val))
                            {
                                slot.valid = false;
                            }
                        }
                    }
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Pass 3: `Ldp`/`Stp` Pairing and Post-Index Writeback Fusion
// -----------------------------------------------------------------------------

#[inline]
const fn is_pairable_size(size: MemSize) -> bool {
    matches!(size, MemSize::W32 | MemSize::X64 | MemSize::S32 | MemSize::D64 | MemSize::Q128)
}

#[inline]
fn legal_pair_offset(off: i32, size: MemSize) -> Option<i16> {
    if !is_pairable_size(size) {
        return None;
    }
    let bytes = i32::try_from(size.byte_size()).ok()?;
    if (off % bytes) != 0 {
        return None;
    }
    let scaled = off / bytes;
    if !(-64..=63).contains(&scaled) {
        return None;
    }
    i16::try_from(off).ok()
}

/// True if instruction `k` may alias a memory access at `(base, off, bytes)` as a write (or also
/// as a read when `check_reads` is `true`, used when reordering a store).
fn inst_may_alias_mem(
    func: &MFunc<'_>,
    k: usize,
    base: MReg,
    off: i32,
    bytes: u32,
    check_reads: bool,
) -> bool {
    if is_volatile_inst(func, k) || is_call_or_mem_barrier(func, k) {
        return true;
    }
    let op = op_at(func, k);
    if matches!(op, MOp::RawWord { .. } | MOp::ParallelCopy { .. }) || is_control_or_call(&op) {
        return true;
    }
    let fl = flags_at(func, k);
    if fl.contains(MInstFlags::MAY_TRAP) {
        return true;
    }
    match op {
        MOp::Str { addr: AddrMode::BaseOff { base: sb, off: so }, size, .. } => {
            sb != base || byte_ranges_overlap(so, size.byte_size(), off, bytes)
        }
        MOp::Stp { base: sb, off: so, size, post_idx: false, pre_idx: false, .. } => {
            sb != base
                || byte_ranges_overlap(
                    i32::from(so),
                    size.byte_size().saturating_mul(2),
                    off,
                    bytes,
                )
        }
        MOp::Str { .. } | MOp::Stp { .. } => true,
        MOp::Ldr { addr: AddrMode::BaseOff { base: lb, off: lo }, size, .. } => {
            check_reads && (lb != base || byte_ranges_overlap(lo, size.byte_size(), off, bytes))
        }
        MOp::Ldp { base: lb, off: lo, size, post_idx: false, pre_idx: false, .. } => {
            check_reads
                && (lb != base
                    || byte_ranges_overlap(
                        i32::from(lo),
                        size.byte_size().saturating_mul(2),
                        off,
                        bytes,
                    ))
        }
        MOp::Ldr { .. } | MOp::Ldp { .. } => check_reads,
        _ => fl.contains(MInstFlags::MEM_WRITE) || (check_reads && fl.contains(MInstFlags::MEM_READ)),
    }
}

/// Pass 3: pairs nearby non-aliasing `Ldr`/`Str` ops into `Ldp`/`Stp` (window `<= 12`), then
/// fuses zero-offset `Ldr`/`Str`/`Ldp`/`Stp` + `AddImm`/`SubImm` into `PostIdx` writeback.
fn mopt_pair_ldp_stp_and_post_index<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
    stats: &mut MOptStats,
) {
    let pairs_before = stats.pairs_formed;
    mopt_pair_ldp_stp(func, stats);
    if stats.pairs_formed > pairs_before {
        compact_nops(func, alloc);
    }
    mopt_fuse_post_index(func, stats);
}

fn mopt_pair_ldp_stp(func: &mut MFunc<'_>, stats: &mut MOptStats) {
    let n_order = func.block_order.len();
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        if start >= end {
            continue;
        }

        for i in start..end {
            if is_protected(func, i) || is_volatile_inst(func, i) {
                continue;
            }
            match op_at(func, i) {
                MOp::Ldr {
                    dst: dst_i,
                    addr: AddrMode::BaseOff { base, off: off_i },
                    size,
                    signed: false,
                } if is_pairable_size(size) && dst_i != base => {
                    let elem = i32::try_from(size.byte_size()).unwrap_or(8);
                    let scan_end = end.min(i.saturating_add(PAIR_WINDOW));
                    for j in (i + 1)..scan_end {
                        let op_j = op_at(func, j);
                        if matches!(op_j, MOp::Nop) {
                            continue;
                        }
                        if !is_protected(func, j)
                            && !is_volatile_inst(func, j)
                            && let MOp::Ldr {
                                dst: dst_j,
                                addr: AddrMode::BaseOff { base: base_j, off: off_j },
                                size: size_j,
                                signed: false,
                            } = op_j
                            && base_j == base
                            && size_j == size
                            && dst_i != dst_j
                        {
                            let pair_order = if off_i.checked_add(elem) == Some(off_j) {
                                Some((dst_i, dst_j, off_i))
                            } else if off_j.checked_add(elem) == Some(off_i) {
                                Some((dst_j, dst_i, off_j))
                            } else {
                                None
                            };
                            if let Some((r0, r1, low_off)) = pair_order
                                && let Some(off_i16) = legal_pair_offset(low_off, size)
                            {
                                // Check whether we can hoist `j` to `i` OR sink `i` to `j`.
                                let mut can_hoist_j = true;
                                let mut can_sink_i = true;
                                for k in (i + 1)..j {
                                    if matches!(op_at(func, k), MOp::Nop) {
                                        continue;
                                    }
                                    if inst_defines_reg(func, k, base) {
                                        can_hoist_j = false;
                                        can_sink_i = false;
                                        break;
                                    }
                                    if inst_touches_reg(func, k, dst_j)
                                        || inst_may_alias_mem(
                                            func,
                                            k,
                                            base,
                                            off_j,
                                            size.byte_size(),
                                            false,
                                        )
                                    {
                                        can_hoist_j = false;
                                    }
                                    if inst_touches_reg(func, k, dst_i)
                                        || inst_may_alias_mem(
                                            func,
                                            k,
                                            base,
                                            off_i,
                                            size.byte_size(),
                                            false,
                                        )
                                    {
                                        can_sink_i = false;
                                    }
                                }

                                let ldp = MOp::Ldp {
                                    dst0: r0,
                                    dst1: r1,
                                    base,
                                    off: off_i16,
                                    size,
                                    post_idx: false,
                                    pre_idx: false,
                                };
                                if can_hoist_j {
                                    set(&mut func.ops, i, ldp);
                                    set(&mut func.flags, i, MInstFlags::MEM_READ);
                                    mark_deleted(func, j);
                                    stats.pairs_formed = stats.pairs_formed.saturating_add(1);
                                    break;
                                } else if can_sink_i {
                                    mark_deleted(func, i);
                                    set(&mut func.ops, j, ldp);
                                    set(&mut func.flags, j, MInstFlags::MEM_READ);
                                    stats.pairs_formed = stats.pairs_formed.saturating_add(1);
                                    break;
                                }
                            }
                        }

                        if inst_defines_reg(func, j, base)
                            || is_control_or_call(&op_j)
                            || is_call_or_mem_barrier(func, j)
                        {
                            break;
                        }
                    }
                }
                MOp::Str {
                    src: src_i,
                    addr: AddrMode::BaseOff { base, off: off_i },
                    size,
                } if is_pairable_size(size) => {
                    let elem = i32::try_from(size.byte_size()).unwrap_or(8);
                    let scan_end = end.min(i.saturating_add(PAIR_WINDOW));
                    for j in (i + 1)..scan_end {
                        let op_j = op_at(func, j);
                        if matches!(op_j, MOp::Nop) {
                            continue;
                        }
                        if !is_protected(func, j)
                            && !is_volatile_inst(func, j)
                            && let MOp::Str {
                                src: src_j,
                                addr: AddrMode::BaseOff { base: base_j, off: off_j },
                                size: size_j,
                            } = op_j
                            && base_j == base
                            && size_j == size
                        {
                            let pair_order = if off_i.checked_add(elem) == Some(off_j) {
                                Some((src_i, src_j, off_i))
                            } else if off_j.checked_add(elem) == Some(off_i) {
                                Some((src_j, src_i, off_j))
                            } else {
                                None
                            };
                            if let Some((r0, r1, low_off)) = pair_order
                                && let Some(off_i16) = legal_pair_offset(low_off, size)
                            {
                                let mut can_sink_i = true;
                                let mut can_hoist_j = true;
                                for k in (i + 1)..j {
                                    if matches!(op_at(func, k), MOp::Nop) {
                                        continue;
                                    }
                                    if inst_defines_reg(func, k, base) {
                                        can_sink_i = false;
                                        can_hoist_j = false;
                                        break;
                                    }
                                    if inst_defines_reg(func, k, src_i)
                                        || inst_may_alias_mem(
                                            func,
                                            k,
                                            base,
                                            off_i,
                                            size.byte_size(),
                                            true,
                                        )
                                    {
                                        can_sink_i = false;
                                    }
                                    if inst_defines_reg(func, k, src_j)
                                        || inst_may_alias_mem(
                                            func,
                                            k,
                                            base,
                                            off_j,
                                            size.byte_size(),
                                            true,
                                        )
                                    {
                                        can_hoist_j = false;
                                    }
                                }

                                let stp = MOp::Stp {
                                    src0: r0,
                                    src1: r1,
                                    base,
                                    off: off_i16,
                                    size,
                                    post_idx: false,
                                    pre_idx: false,
                                };
                                if can_sink_i {
                                    mark_deleted(func, i);
                                    set(&mut func.ops, j, stp);
                                    set(&mut func.flags, j, MInstFlags::MEM_WRITE);
                                    stats.pairs_formed = stats.pairs_formed.saturating_add(1);
                                    break;
                                } else if can_hoist_j {
                                    set(&mut func.ops, i, stp);
                                    set(&mut func.flags, i, MInstFlags::MEM_WRITE);
                                    mark_deleted(func, j);
                                    stats.pairs_formed = stats.pairs_formed.saturating_add(1);
                                    break;
                                }
                            }
                        }

                        if inst_defines_reg(func, j, base)
                            || is_control_or_call(&op_j)
                            || is_call_or_mem_barrier(func, j)
                        {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn mopt_fuse_post_index(func: &mut MFunc<'_>, stats: &mut MOptStats) {
    let n_order = func.block_order.len();
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        if start >= end {
            continue;
        }

        for i in start..end {
            if is_protected(func, i) || is_volatile_inst(func, i) {
                continue;
            }
            let (base, size, is_pair) = match op_at(func, i) {
                MOp::Ldr { dst, addr: AddrMode::BaseOff { base, off: 0 }, size, .. }
                    if base != MReg::SP && (size.is_fp_or_vec() || dst != base) =>
                {
                    (base, size, false)
                }
                MOp::Str { src, addr: AddrMode::BaseOff { base, off: 0 }, size }
                    if base != MReg::SP && (size.is_fp_or_vec() || src != base) =>
                {
                    (base, size, false)
                }
                MOp::Ldp {
                    dst0,
                    dst1,
                    base,
                    off: 0,
                    size,
                    post_idx: false,
                    pre_idx: false,
                } if is_pairable_size(size)
                    && base != MReg::SP
                    && dst0 != dst1
                    && (size.is_fp_or_vec() || (dst0 != base && dst1 != base)) =>
                {
                    (base, size, true)
                }
                MOp::Stp {
                    src0,
                    src1,
                    base,
                    off: 0,
                    size,
                    post_idx: false,
                    pre_idx: false,
                } if is_pairable_size(size)
                    && base != MReg::SP
                    && (size.is_fp_or_vec() || (src0 != base && src1 != base)) =>
                {
                    (base, size, true)
                }
                _ => continue,
            };

            // In SSA mode, only fuse in-place base updates when `base` is not a virtual register
            // (preserving SSA single-assignment invariants cleanly).
            if func.is_ssa && base.is_virtual() {
                continue;
            }

            let scan_end = end.min(i.saturating_add(POST_IDX_WINDOW));
            for j in (i + 1)..scan_end {
                if note_at(func, j) != NO_NOTE {
                    break;
                }
                let op_j = op_at(func, j);
                if matches!(op_j, MOp::Nop) {
                    continue;
                }
                if !is_protected(func, j) {
                    let step_opt: Option<i32> = match op_j {
                        MOp::AddImm {
                            dst,
                            rn,
                            imm12,
                            shift12: false,
                            wide: true,
                            sets_flags: false,
                        } if dst == base && rn == base && imm12 != 0 => Some(i32::from(imm12)),
                        MOp::SubImm {
                            dst,
                            rn,
                            imm12,
                            shift12: false,
                            wide: true,
                            sets_flags: false,
                        } if dst == base && rn == base && imm12 != 0 => Some(-i32::from(imm12)),
                        _ => None,
                    };

                    if let Some(step) = step_opt {
                        let legal_step: Option<i16> = if is_pair {
                            legal_pair_offset(step, size)
                        } else if (-256..=255).contains(&step) {
                            i16::try_from(step).ok()
                        } else {
                            None
                        };

                        if let Some(step_i16) = legal_step {
                            if let Some(op_i_mut) = func.ops.get_mut(i) {
                                match op_i_mut {
                                    MOp::Ldr { addr, .. } | MOp::Str { addr, .. } => {
                                        *addr = AddrMode::PostIdx { base, off: step_i16 };
                                    }
                                    MOp::Ldp { off, post_idx, .. }
                                    | MOp::Stp { off, post_idx, .. } => {
                                        *off = step_i16;
                                        *post_idx = true;
                                    }
                                    _ => {}
                                }
                            }
                            mark_deleted(func, j);
                            stats.post_idx_formed = stats.post_idx_formed.saturating_add(1);
                        }
                        break;
                    }
                }

                // Any intervening touch of `base`, branch, call, trap, or barrier blocks hoisting
                // the base pointer update up into `i`.
                if inst_touches_reg(func, j, base)
                    || is_control_or_call(&op_j)
                    || is_call_or_mem_barrier(func, j)
                    || flags_at(func, j).contains(MInstFlags::MAY_TRAP)
                    || matches!(op_j, MOp::RawWord { .. } | MOp::ParallelCopy { .. })
                {
                    break;
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Pass 4: Branch Threading, Conditional Inversion, Fallthrough Elision & CFG Rebuild
// -----------------------------------------------------------------------------

/// Returns `Some(target)` iff `blk` is an empty pass-through block containing either:
/// - a single unprotected `MOp::B { target }` (with no other noted/pinned instructions), OR
/// - zero instructions (no noted/pinned `Nop`s) falling through to `next_in_order[blk]`.
fn pass_through_target(
    func: &MFunc<'_>,
    next_in_order: &[u32],
    blk: MBlock,
) -> Option<MBlock> {
    let b = to_idx(blk.0);
    let &(start_u, end_u) = func.block_insts.get(b)?;
    let start = to_idx(start_u);
    let end = to_idx(end_u).min(func.ops.len());

    let mut active_idx: Option<usize> = None;
    for i in start..end {
        if matches!(op_at(func, i), MOp::Nop) {
            if note_at(func, i) != NO_NOTE || flags_at(func, i).contains(MInstFlags::PINNED) {
                return None;
            }
            continue;
        }
        if active_idx.is_some() {
            return None;
        }
        active_idx = Some(i);
    }

    match active_idx {
        Some(idx) => {
            if is_protected(func, idx) {
                return None;
            }
            if let MOp::B { target } = op_at(func, idx) {
                Some(target)
            } else {
                None
            }
        }
        None => {
            let next = next_in_order.get(b).copied().unwrap_or(u32::MAX);
            if next != u32::MAX { Some(MBlock(next)) } else { None }
        }
    }
}

fn thread_branch_target(func: &MFunc<'_>, next_in_order: &[u32], orig: MBlock) -> MBlock {
    let mut cur = orig;
    for _ in 0..MAX_THREAD_HOPS {
        let Some(next) = pass_through_target(func, next_in_order, cur) else {
            break;
        };
        if next == cur || next == orig {
            break;
        }
        cur = next;
    }
    cur
}

fn set_branch_target(op: &mut MOp, new_target: MBlock) {
    match op {
        MOp::B { target }
        | MOp::BCond { target, .. }
        | MOp::Cbz { target, .. }
        | MOp::Cbnz { target, .. }
        | MOp::Tbz { target, .. }
        | MOp::Tbnz { target, .. } => {
            *target = new_target;
        }
        _ => {}
    }
}

#[inline]
const fn invert_cond_branch(op: MOp, new_target: MBlock) -> Option<MOp> {
    match op {
        MOp::BCond { cond, .. } if !matches!(cond, MCond::Al | MCond::Nv) => {
            Some(MOp::BCond { cond: cond.invert(), target: new_target })
        }
        MOp::Cbz { rt, wide, .. } => Some(MOp::Cbnz { rt, target: new_target, wide }),
        MOp::Cbnz { rt, wide, .. } => Some(MOp::Cbz { rt, target: new_target, wide }),
        MOp::Tbz { rt, bit, .. } => Some(MOp::Tbnz { rt, bit, target: new_target }),
        MOp::Tbnz { rt, bit, .. } => Some(MOp::Tbz { rt, bit, target: new_target }),
        _ => None,
    }
}

/// Returns `true` iff falling through into `start_blk` is guaranteed to reach `target`
/// via zero or more empty / unprotected pass-through `B` blocks (bounded by `MAX_THREAD_HOPS`),
/// without revisiting `forbid_blk`.
fn fallthrough_reaches(
    func: &MFunc<'_>,
    next_in_order: &[u32],
    start_blk: MBlock,
    target: MBlock,
    forbid_blk: MBlock,
) -> bool {
    let mut cur = start_blk;
    for _ in 0..=MAX_THREAD_HOPS {
        if cur == target {
            return true;
        }
        if cur == forbid_blk {
            return false;
        }
        let Some(next) = pass_through_target(func, next_in_order, cur) else {
            return false;
        };
        if next == cur {
            return false;
        }
        cur = next;
    }
    false
}

/// Returns `true` iff `blk`'s last non-NOP instruction is an unconditional terminator (so `blk`
/// never falls through to the next block in `func.block_order`). Returns `false` for empty blocks.
fn block_ends_with_uncond_term(func: &MFunc<'_>, blk: MBlock) -> bool {
    let b = to_idx(blk.0);
    let Some(&(start_u, end_u)) = func.block_insts.get(b) else {
        return false;
    };
    let start = to_idx(start_u);
    let end = to_idx(end_u).min(func.ops.len());
    for i in (start..end).rev() {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }
        return op.is_unconditional_terminator();
    }
    false
}

/// Returns `true` iff `blk` contains `1..=MAX_SECONDARY_CO_LOCATE_P_INSTS` non-NOP instructions
/// and has no branch, terminator, or opaque `RawWord` instructions (a short pure fallthrough copy
/// block that a secondary `S -> B T` block can co-locate immediately ahead of).
fn is_short_pure_fallthrough_block(func: &MFunc<'_>, blk: MBlock) -> bool {
    let b = to_idx(blk.0);
    let Some(&(start_u, end_u)) = func.block_insts.get(b) else {
        return false;
    };
    let start = to_idx(start_u);
    let end = to_idx(end_u).min(func.ops.len());
    let mut non_nop_count: usize = 0;
    for i in start..end {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }
        if op.is_conditional_branch()
            || op.is_unconditional_terminator()
            || matches!(op, MOp::RawWord { .. })
        {
            return false;
        }
        non_nop_count += 1;
        if non_nop_count > MAX_SECONDARY_CO_LOCATE_P_INSTS {
            return false;
        }
    }
    non_nop_count > 0
}

/// Returns `true` iff `blk` contains zero non-NOP instructions (and no pinned/noted `Nop`s), OR a
/// single unprotected `MOp::B` instruction (an empty trampoline left behind by branch threading).
fn is_empty_or_unprotected_trampoline(func: &MFunc<'_>, blk: MBlock) -> bool {
    let b = to_idx(blk.0);
    let Some(&(start_u, end_u)) = func.block_insts.get(b) else {
        return false;
    };
    let start = to_idx(start_u);
    let end = to_idx(end_u).min(func.ops.len());
    let mut non_nop_idx: Option<usize> = None;
    for i in start..end {
        if matches!(op_at(func, i), MOp::Nop) {
            if note_at(func, i) != NO_NOTE || flags_at(func, i).contains(MInstFlags::PINNED) {
                return false;
            }
            continue;
        }
        if non_nop_idx.is_some() {
            return false;
        }
        non_nop_idx = Some(i);
    }
    match non_nop_idx {
        None => true,
        Some(idx) => !is_protected(func, idx) && matches!(op_at(func, idx), MOp::B { .. }),
    }
}

/// Rebuilds `next_in_order` and `block_ord_pos` from the current `func.block_order`.
fn refresh_order_maps(
    func: &MFunc<'_>,
    next_in_order: &mut [u32],
    block_ord_pos: &mut [u32],
) {
    let num_blocks = func.block_insts.len();
    for slot in next_in_order.iter_mut() {
        *slot = u32::MAX;
    }
    for slot in block_ord_pos.iter_mut() {
        *slot = u32::MAX;
    }
    for pair in func.block_order.windows(2) {
        if let &[a, b_next] = pair {
            let b = to_idx(a.0);
            if b < num_blocks {
                set(next_in_order, b, b_next.0);
            }
        }
    }
    for (pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        if b < num_blocks {
            set(block_ord_pos, b, u32_of(pos).unwrap_or(u32::MAX));
        }
    }
}

/// Pass 4b helper: conditional-branch inversion over trailing `B` + redundant fallthrough `B` elision.
fn mopt_branch_fold_fallthrough(
    func: &mut MFunc<'_>,
    next_in_order: &[u32],
    block_ord_pos: &[u32],
    stats: &mut MOptStats,
) -> bool {
    let mut changed = false;
    let n_order = func.block_order.len();
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let next_id = next_in_order.get(to_idx(blk.0)).copied().unwrap_or(u32::MAX);
        if next_id == u32::MAX {
            // Last block in `block_order` MUST keep its unconditional terminator (`mverify`).
            continue;
        }
        let next_blk = MBlock(next_id);
        let (start, end) = block_range(func, blk);

        let mut last_idx: Option<usize> = None;
        let mut prev_idx: Option<usize> = None;
        for i in (start..end).rev() {
            if matches!(op_at(func, i), MOp::Nop) {
                continue;
            }
            if last_idx.is_none() {
                last_idx = Some(i);
            } else {
                prev_idx = Some(i);
                break;
            }
        }

        let Some(last) = last_idx else {
            continue;
        };
        let op_last = op_at(func, last);

        // Case 1: Trailing `B { target: uncond_target }`.
        if !is_protected(func, last)
            && let MOp::B { target: uncond_target } = op_last
        {
            if fallthrough_reaches(func, next_in_order, next_blk, uncond_target, blk) {
                // Redundant fallthrough `B`!
                mark_deleted(func, last);
                stats.branches_folded = stats.branches_folded.saturating_add(1);
                changed = true;

                // If the preceding conditional branch ALSO reaches fallthrough, delete it too.
                if let Some(prev) = prev_idx
                    && !is_protected(func, prev)
                    && op_at(func, prev).is_conditional_branch()
                    && let Some(prev_target) = op_at(func, prev).branch_target()
                    && fallthrough_reaches(func, next_in_order, next_blk, prev_target, blk)
                {
                    mark_deleted(func, prev);
                    stats.branches_folded = stats.branches_folded.saturating_add(1);
                }
                continue;
            }

            // If `prev` is a conditional branch targeting the exact same block as trailing `B`,
            // both paths reach `uncond_target` so the conditional branch is dead.
            if let Some(prev) = prev_idx
                && !is_protected(func, prev)
                && op_at(func, prev).is_conditional_branch()
                && op_at(func, prev).branch_target() == Some(uncond_target)
            {
                mark_deleted(func, prev);
                stats.branches_folded = stats.branches_folded.saturating_add(1);
                changed = true;
                continue;
            }

            // Invert `cond_br -> cond_target; B uncond_target` (preserving `cond_br`'s `vflag_use`
            // and `MInstFlags::TERMINATOR`):
            // 1. Fallthrough inversion: `cond_target` reaches `next_blk` -> delete trailing `B`.
            // 2. Hot-backedge inversion: `uncond_target` is a backward loop back-edge
            //    (`uncond_pos <= ord_pos`) and either `cond_target` is a forward exit
            //    (`cond_pos > ord_pos`) or `uncond_target` is a self-loop (`uncond_pos == ord_pos`,
            //    whose inner back-edge is tighter than an outer-loop latch at `cond_pos < ord_pos`)
            //    -> invert `cond_br` to target `uncond_target` and rewrite trailing `B` to target
            //    `cond_target` (1 taken branch + macro-op fusion per iter!).
            if let Some(prev) = prev_idx
                && !is_protected(func, prev)
                && op_at(func, prev).is_conditional_branch()
                && let Some(cond_target) = op_at(func, prev).branch_target()
            {
                if fallthrough_reaches(func, next_in_order, next_blk, cond_target, blk) {
                    if let Some(inv_op) = invert_cond_branch(op_at(func, prev), uncond_target) {
                        set(&mut func.ops, prev, inv_op);
                        mark_deleted(func, last);
                        stats.branches_folded = stats.branches_folded.saturating_add(1);
                        changed = true;
                    }
                } else if uncond_target != cond_target
                    && let Some(ord_pos) = u32_of(bi)
                {
                    let uncond_pos =
                        block_ord_pos.get(to_idx(uncond_target.0)).copied().unwrap_or(u32::MAX);
                    let cond_pos =
                        block_ord_pos.get(to_idx(cond_target.0)).copied().unwrap_or(u32::MAX);
                    if uncond_pos <= ord_pos
                        && (cond_pos > ord_pos || uncond_pos == ord_pos)
                        && cond_pos != u32::MAX
                        && let Some(inv_op) = invert_cond_branch(op_at(func, prev), uncond_target)
                    {
                        set(&mut func.ops, prev, inv_op);
                        set(&mut func.ops, last, MOp::B { target: cond_target });
                        stats.branches_folded = stats.branches_folded.saturating_add(1);
                        changed = true;
                    }
                }
            }
            continue;
        }

        // Case 2: Trailing conditional branch reaching fallthrough (both paths reach the same block).
        if !is_protected(func, last)
            && op_last.is_conditional_branch()
            && let Some(cond_target) = op_last.branch_target()
            && fallthrough_reaches(func, next_in_order, next_blk, cond_target, blk)
        {
            mark_deleted(func, last);
            stats.branches_folded = stats.branches_folded.saturating_add(1);
            changed = true;
        }
    }
    changed
}

/// Returns `true` iff every instruction in `blk` has zero attached `MNote`s.
fn block_has_no_notes(func: &MFunc<'_>, blk: MBlock) -> bool {
    let (start, end) = block_range(func, blk);
    for i in start..end {
        if note_at(func, i) != NO_NOTE {
            return false;
        }
    }
    true
}

/// Returns `true` iff no instruction in `blk` carries `MInstFlags::PINNED`.
fn block_has_no_pinned(func: &MFunc<'_>, blk: MBlock) -> bool {
    let (start, end) = block_range(func, blk);
    for i in start..end {
        if flags_at(func, i).contains(MInstFlags::PINNED) {
            return false;
        }
    }
    true
}

/// Shifts `count` (`1` or `2`) contiguous blocks starting at `src_start` in `order` so that they
/// land immediately before `dst_before` (zero allocation, zero unchecked indexing).
fn move_block_slice_before(
    order: &mut [MBlock],
    src_start: usize,
    count: usize,
    dst_before: usize,
) {
    let n = order.len();
    if count == 0 || count > 2 || src_start.saturating_add(count) > n || dst_before > n {
        return;
    }
    let saved0 = at(order, src_start, MBlock(0));
    let saved1 = if count == 2 { at(order, src_start + 1, MBlock(0)) } else { MBlock(0) };
    if dst_before < src_start {
        let mut k = src_start;
        while k > dst_before {
            k -= 1;
            let blk = at(order, k, MBlock(0));
            set(order, k + count, blk);
        }
        set(order, dst_before, saved0);
        if count == 2 {
            set(order, dst_before + 1, saved1);
        }
    } else if dst_before > src_start + count {
        let src_end = src_start + count;
        for k in src_end..dst_before {
            let blk = at(order, k, MBlock(0));
            set(order, k - count, blk);
        }
        let insert_at = dst_before - count;
        set(order, insert_at, saved0);
        if count == 2 {
            set(order, insert_at + 1, saved1);
        }
    }
}

/// Pass 4c helper: prunes unreachable empty / unprotected-trampoline blocks from `func.block_order`
/// (never dropping `block_order[0]`, keeping every target/fallthrough reachable from any kept
/// block), compacts NOPs, folds newly exposed fallthroughs, and rebuilds CFG CSRs.
fn prune_unreachable_empty_blocks<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
    next_in_order: &mut [u32],
    block_ord_pos: &mut [u32],
    stats: &mut MOptStats,
) -> bool {
    let num_blocks = func.block_insts.len();
    if func.block_order.len() <= 1 || num_blocks == 0 {
        return false;
    }
    if func.block_succs_off.len() != num_blocks + 1 {
        func.rebuild_cfg(alloc);
    }

    let mut keep: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        keep.push(false);
    }
    let mut worklist: Vec<MBlock, Alloc<'a>> = Vec::with_capacity_in(func.block_order.len(), alloc);

    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        if b < num_blocks
            && (ord_pos == 0 || !is_empty_or_unprotected_trampoline(func, blk))
            && !at(&keep, b, true)
        {
            set(&mut keep, b, true);
            worklist.push(blk);
        }
    }

    while let Some(cur) = worklist.pop() {
        for &succ in func.block_succs(cur) {
            let s = to_idx(succ.0);
            if s < num_blocks && !at(&keep, s, true) {
                set(&mut keep, s, true);
                worklist.push(succ);
            }
        }
    }

    let mut write_pos: usize = 0;
    let orig_len = func.block_order.len();
    for read_pos in 0..orig_len {
        let Some(&blk) = func.block_order.get(read_pos) else {
            continue;
        };
        let b = to_idx(blk.0);
        if read_pos == 0 || keep.get(b).copied().unwrap_or(true) {
            set(&mut func.block_order, write_pos, blk);
            write_pos += 1;
        } else {
            let mut deleted_b = false;
            let (s_i, e_i) = block_range(func, blk);
            for i in s_i..e_i {
                if !is_protected(func, i) {
                    if matches!(op_at(func, i), MOp::B { .. }) {
                        deleted_b = true;
                    }
                    mark_deleted(func, i);
                }
            }
            if deleted_b {
                stats.branches_folded = stats.branches_folded.saturating_add(1);
            }
        }
    }

    if write_pos < orig_len {
        func.block_order.truncate(write_pos);
        compact_nops(func, alloc);
        refresh_order_maps(func, next_in_order, block_ord_pos);
        if mopt_branch_fold_fallthrough(func, next_in_order, block_ord_pos, stats) {
            compact_nops(func, alloc);
        }
        func.rebuild_cfg(alloc);
        return true;
    }
    false
}

#[derive(Clone, Copy)]
struct TailMergeCand {
    blk: MBlock,
    ord_pos: u32,
    target: MBlock,
    prefix: [MOp; MAX_TAIL_MERGE_OPS],
    prefix_len: u8,
    is_ft_to_target: bool,
    has_ft_pred: bool,
}

#[inline]
fn tail_merge_prefix_eq(a: &TailMergeCand, b: &TailMergeCand) -> bool {
    if a.prefix_len != b.prefix_len || a.target != b.target {
        return false;
    }
    let len = usize::from(a.prefix_len).min(MAX_TAIL_MERGE_OPS);
    for k in 0..len {
        if at(&a.prefix, k, MOp::Nop) != at(&b.prefix, k, MOp::Nop) {
            return false;
        }
    }
    true
}

#[inline]
fn tail_merge_canon_rank(c: &TailMergeCand, target_pos: u32) -> (u8, u32) {
    let adj_to_target = target_pos != u32::MAX && c.ord_pos.saturating_add(1) == target_pos;
    let tier: u8 = if c.is_ft_to_target {
        0
    } else if c.has_ft_pred && adj_to_target {
        1
    } else if c.has_ft_pred {
        2
    } else if adj_to_target {
        3
    } else {
        4
    };
    (tier, c.ord_pos)
}

/// Pass 4e (post-RA only, no jump tables or unwind notes): merges identical short non-entry tail
/// blocks (`1..=12` non-NOP ops ending in `MOp::B { target }`, or a matching `1..=11`-op pure
/// fallthrough block immediately before `target`) into one canonical block, redirecting all
/// branches targeting duplicates and clearing duplicate ops so Pass 4c prunes them.
fn mopt_branch_tail_merge_post_ra<'a>(
    func: &mut MFunc<'a>,
    alloc: Alloc<'a>,
    next_in_order: &[u32],
    block_ord_pos: &[u32],
    stats: &mut MOptStats,
) -> bool {
    let num_blocks = func.block_insts.len();
    let n_order = func.block_order.len();
    if func.is_ssa || num_blocks == 0 || n_order <= 2 {
        return false;
    }

    // Never redirect or delete a block targeted by a protected (`PINNED`/noted) branch.
    let mut has_protected_ref: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        has_protected_ref.push(false);
    }
    for i in 0..func.ops.len() {
        if is_protected(func, i)
            && let Some(target) = op_at(func, i).branch_target()
        {
            set(&mut has_protected_ref, to_idx(target.0), true);
        }
    }

    let mut cands: Vec<TailMergeCand, Alloc<'a>> = Vec::new_in(alloc);
    for ord_pos in 1..n_order {
        let Some(&blk) = func.block_order.get(ord_pos) else {
            continue;
        };
        let Some(&prev_blk) = func.block_order.get(ord_pos - 1) else {
            continue;
        };
        let has_ft_pred = !block_ends_with_uncond_term(func, prev_blk);
        let (start, end) = block_range(func, blk);

        let mut ops = [MOp::Nop; MAX_TAIL_MERGE_OPS];
        let mut count: usize = 0;
        let mut eligible = true;
        for i in start..end {
            if note_at(func, i) != NO_NOTE {
                eligible = false;
                break;
            }
            let fl = flags_at(func, i);
            if fl.contains(MInstFlags::PINNED)
                || fl.contains(MInstFlags::VOLATILE)
                || fl.contains(MInstFlags::MAY_TRAP)
                || fl.contains(MInstFlags::CALL)
                || fl.contains(MInstFlags::MEM_BARRIER)
            {
                eligible = false;
                break;
            }
            let op = op_at(func, i);
            if matches!(op, MOp::Nop) {
                continue;
            }
            if matches!(op, MOp::RawWord { .. } | MOp::ParallelCopy { .. }) {
                eligible = false;
                break;
            }
            if count >= MAX_TAIL_MERGE_OPS {
                eligible = false;
                break;
            }
            set(&mut ops, count, op);
            count += 1;
        }
        if !eligible || count == 0 {
            continue;
        }

        let last_op = at(&ops, count - 1, MOp::Nop);
        let (target, prefix_len, is_ft_to_target) = if let MOp::B { target } = last_op {
            if target == blk || to_idx(target.0) >= num_blocks {
                continue;
            }
            let mut prefix_ok = true;
            for k in 0..(count - 1) {
                let op_k = at(&ops, k, MOp::Nop);
                if is_control_or_call(&op_k) {
                    prefix_ok = false;
                    break;
                }
            }
            if !prefix_ok {
                continue;
            }
            (target, count - 1, false)
        } else if count <= MAX_TAIL_MERGE_PREFIX_OPS {
            let next_id = at(next_in_order, to_idx(blk.0), u32::MAX);
            if next_id == u32::MAX {
                continue;
            }
            let target = MBlock(next_id);
            if target == blk || to_idx(target.0) >= num_blocks {
                continue;
            }
            let mut prefix_ok = true;
            for k in 0..count {
                let op_k = at(&ops, k, MOp::Nop);
                if is_control_or_call(&op_k) {
                    prefix_ok = false;
                    break;
                }
            }
            if !prefix_ok {
                continue;
            }
            (target, count, true)
        } else {
            continue;
        };

        let mut prefix = [MOp::Nop; MAX_TAIL_MERGE_OPS];
        for k in 0..prefix_len.min(MAX_TAIL_MERGE_PREFIX_OPS) {
            set(&mut prefix, k, at(&ops, k, MOp::Nop));
        }
        let Some(ord_pos_u) = u32_of(ord_pos) else {
            continue;
        };
        let Ok(prefix_len_u8) = u8::try_from(prefix_len) else {
            continue;
        };
        cands.push(TailMergeCand {
            blk,
            ord_pos: ord_pos_u,
            target,
            prefix,
            prefix_len: prefix_len_u8,
            is_ft_to_target,
            has_ft_pred,
        });
    }

    if cands.len() < 2 {
        return false;
    }

    // Group candidates by `target` in ascending `ord_pos` via an O(N) intrusive bucket list.
    let mut first_for_target: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        first_for_target.push(u32::MAX);
    }
    let mut next_in_bucket: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(cands.len(), alloc);
    for _ in 0..cands.len() {
        next_in_bucket.push(u32::MAX);
    }
    for ci in (0..cands.len()).rev() {
        let Some(cand) = cands.get(ci) else {
            continue;
        };
        let ti = to_idx(cand.target.0);
        let prev_head = at(&first_for_target, ti, u32::MAX);
        set(&mut next_in_bucket, ci, prev_head);
        if let Some(ci_u) = u32_of(ci) {
            set(&mut first_for_target, ti, ci_u);
        }
    }

    let mut redirect: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        redirect.push(u32::MAX);
    }
    let mut merged_any = false;

    for t in 0..num_blocks {
        let mut cur = at(&first_for_target, t, u32::MAX);
        if cur == u32::MAX {
            continue;
        }
        let mut bucket_idxs = [0usize; MAX_TAIL_MERGE_BUCKET];
        let mut bucket_len: usize = 0;
        while cur != u32::MAX && bucket_len < MAX_TAIL_MERGE_BUCKET {
            let ci = to_idx(cur);
            set(&mut bucket_idxs, bucket_len, ci);
            bucket_len += 1;
            cur = at(&next_in_bucket, ci, u32::MAX);
        }
        if bucket_len < 2 {
            continue;
        }

        let target_pos = at(block_ord_pos, t, u32::MAX);
        let mut visited = [false; MAX_TAIL_MERGE_BUCKET];
        for a in 0..bucket_len {
            if at(&visited, a, true) {
                continue;
            }
            let ci_a = at(&bucket_idxs, a, 0);
            let Some(&cand_a) = cands.get(ci_a) else {
                continue;
            };
            let mut group = [0usize; MAX_TAIL_MERGE_BUCKET];
            let mut group_len: usize = 0;
            for b in a..bucket_len {
                if at(&visited, b, true) {
                    continue;
                }
                let ci_b = at(&bucket_idxs, b, 0);
                let Some(&cand_b) = cands.get(ci_b) else {
                    continue;
                };
                if tail_merge_prefix_eq(&cand_a, &cand_b) {
                    set(&mut visited, b, true);
                    set(&mut group, group_len, ci_b);
                    group_len += 1;
                }
            }
            if group_len < 2 {
                continue;
            }

            let mut canon = cand_a;
            let mut best_rank = tail_merge_canon_rank(&cand_a, target_pos);
            for g in 1..group_len {
                let ci_g = at(&group, g, 0);
                if let Some(&cand_g) = cands.get(ci_g) {
                    let rank_g = tail_merge_canon_rank(&cand_g, target_pos);
                    if rank_g < best_rank {
                        best_rank = rank_g;
                        canon = cand_g;
                    }
                }
            }

            for g in 0..group_len {
                let ci_g = at(&group, g, 0);
                let Some(&dup) = cands.get(ci_g) else {
                    continue;
                };
                if dup.blk == canon.blk
                    || dup.has_ft_pred
                    || dup.is_ft_to_target
                    || at(&has_protected_ref, to_idx(dup.blk.0), true)
                {
                    continue;
                }
                set(&mut redirect, to_idx(dup.blk.0), canon.blk.0);
                merged_any = true;
            }
        }
    }

    if !merged_any {
        return false;
    }

    // Redirect every non-protected branch targeting a merged duplicate to its canonical block.
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        for i in start..end {
            if is_protected(func, i) {
                continue;
            }
            if let Some(orig) = op_at(func, i).branch_target() {
                let new_t = at(&redirect, to_idx(orig.0), u32::MAX);
                if new_t != u32::MAX && new_t != orig.0 {
                    if let Some(op_mut) = func.ops.get_mut(i) {
                        set_branch_target(op_mut, MBlock(new_t));
                    }
                    stats.branches_folded = stats.branches_folded.saturating_add(1);
                }
            }
        }
    }

    // Clear duplicate blocks to empty so Pass 4c prunes them cleanly.
    for b in 0..num_blocks {
        let canon_id = at(&redirect, b, u32::MAX);
        if canon_id == u32::MAX {
            continue;
        }
        if at(&func.block_align16, b, false) {
            set(&mut func.block_align16, to_idx(canon_id), true);
        }
        let Some(b_u32) = u32_of(b) else {
            continue;
        };
        let (start, end) = block_range(func, MBlock(b_u32));
        for i in start..end {
            if !is_protected(func, i) {
                mark_deleted(func, i);
            }
        }
    }

    true
}

/// Returns `Some(target)` iff `blk` is a short (`1..=MAX_BACKEDGE_SPLIT_INSTS`), noteless,
/// unpinned block ending in an unprotected `MOp::B { target }` whose target sits at or before
/// `max_pos` in `block_order` (a loop back-edge trampoline).
fn is_short_backedge_trampoline(
    func: &MFunc<'_>,
    block_ord_pos: &[u32],
    blk: MBlock,
    max_pos: usize,
) -> Option<MBlock> {
    if !block_has_no_notes(func, blk) || !block_has_no_pinned(func, blk) {
        return None;
    }
    let (start, end) = block_range(func, blk);
    let mut non_nop_count: usize = 0;
    let mut last_idx: Option<usize> = None;
    for i in start..end {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }
        if matches!(op, MOp::RawWord { .. } | MOp::ParallelCopy { .. }) {
            return None;
        }
        if let Some(prev) = last_idx
            && is_control_or_call(&op_at(func, prev))
        {
            return None;
        }
        non_nop_count += 1;
        if non_nop_count > MAX_BACKEDGE_SPLIT_INSTS {
            return None;
        }
        last_idx = Some(i);
    }
    let last = last_idx?;
    if is_protected(func, last) {
        return None;
    }
    let MOp::B { target } = op_at(func, last) else {
        return None;
    };
    if target == blk {
        return None;
    }
    let t_pos_u = at(block_ord_pos, to_idx(target.0), u32::MAX);
    if t_pos_u == u32::MAX || to_idx(t_pos_u) > max_pos {
        return None;
    }
    Some(target)
}

/// Pass 4f (post-RA only, no jump tables or unwind notes): relocates a short displaced hot loop
/// header `H` (`2..=8` non-NOP insts, no notes, ending in `[cond_br Body, B Exit]`) — together
/// with an optional short pure fallthrough predecessor `prev_H` (`1..=4` ALU ops) — immediately
/// before its fallthrough-free `Body`, then inverts `H` to `invert(cond_br) Exit` falling straight
/// through into `Body`.
fn mopt_branch_relocate_hot_loop_header_post_ra(
    func: &mut MFunc<'_>,
    entry_blk: MBlock,
    next_in_order: &mut [u32],
    block_ord_pos: &mut [u32],
    stats: &mut MOptStats,
) -> bool {
    if func.is_ssa || func.block_order.len() <= 2 {
        return false;
    }
    let mut header_relocs: usize = 0;
    let mut moved_any = false;

    while header_relocs < MAX_HEADER_RELOCATIONS {
        let mut did_move = false;
        let n_ord = func.block_order.len();
        for h_pos in 1..n_ord {
            let Some(&blk_h) = func.block_order.get(h_pos) else {
                continue;
            };
            if blk_h == entry_blk
                || !block_has_no_notes(func, blk_h)
                || !block_has_no_pinned(func, blk_h)
            {
                continue;
            }

            let (start, end) = block_range(func, blk_h);
            let mut non_nop_count: usize = 0;
            let mut prev_idx: Option<usize> = None;
            let mut last_idx: Option<usize> = None;
            let mut clean_ops = true;
            for i in start..end {
                let op = op_at(func, i);
                if matches!(op, MOp::Nop) {
                    continue;
                }
                if matches!(op, MOp::RawWord { .. } | MOp::ParallelCopy { .. }) {
                    clean_ops = false;
                    break;
                }
                non_nop_count += 1;
                if non_nop_count > MAX_HEADER_RELOC_INSTS {
                    clean_ops = false;
                    break;
                }
                prev_idx = last_idx;
                last_idx = Some(i);
            }
            if !clean_ops || non_nop_count < 2 {
                continue;
            }
            let (Some(prev), Some(last)) = (prev_idx, last_idx) else {
                continue;
            };
            if is_protected(func, prev) || is_protected(func, last) {
                continue;
            }
            let MOp::B { target: blk_exit } = op_at(func, last) else {
                continue;
            };
            let op_prev = op_at(func, prev);
            if !op_prev.is_conditional_branch() {
                continue;
            }
            let Some(blk_body) = op_prev.branch_target() else {
                continue;
            };
            let Some(inv_op) = invert_cond_branch(op_prev, blk_exit) else {
                continue;
            };
            if blk_body == blk_h || blk_body == blk_exit || blk_body == entry_blk {
                continue;
            }

            let body_pos_u = at(block_ord_pos, to_idx(blk_body.0), u32::MAX);
            if body_pos_u == u32::MAX || body_pos_u == 0 {
                continue;
            }
            let body_pos = to_idx(body_pos_u);
            if body_pos >= n_ord || h_pos + 1 == body_pos {
                continue;
            }
            if body_pos >= h_pos && body_pos.saturating_sub(h_pos) <= 4 {
                continue;
            }
            let Some(&prev_body) = func.block_order.get(body_pos - 1) else {
                continue;
            };
            if !block_ends_with_uncond_term(func, prev_body) {
                continue;
            }

            let Some(&prev_h) = func.block_order.get(h_pos - 1) else {
                continue;
            };
            let (move_start, move_len) = if block_ends_with_uncond_term(func, prev_h) {
                (h_pos, 1usize)
            } else if h_pos >= 2
                && prev_h != blk_body
                && prev_h != entry_blk
                && let Some(&prev_prev_h) = func.block_order.get(h_pos - 2)
                && block_ends_with_uncond_term(func, prev_prev_h)
                && is_short_pure_fallthrough_block(func, prev_h)
                && block_has_no_notes(func, prev_h)
                && block_has_no_pinned(func, prev_h)
            {
                (h_pos - 1, 2usize)
            } else {
                continue;
            };

            move_block_slice_before(&mut func.block_order, move_start, move_len, body_pos);
            set(&mut func.ops, prev, inv_op);
            mark_deleted(func, last);
            stats.branches_folded = stats.branches_folded.saturating_add(1);
            refresh_order_maps(func, next_in_order, block_ord_pos);
            header_relocs += 1;
            moved_any = true;
            did_move = true;
            break;
        }
        if !did_move {
            break;
        }
    }

    moved_any
}

/// Returns `true` iff `blk`'s last non-NOP instruction is `MOp::Ret`.
fn block_ends_with_ret(func: &MFunc<'_>, blk: MBlock) -> bool {
    let (start, end) = block_range(func, blk);
    for i in (start..end).rev() {
        let op = op_at(func, i);
        if matches!(op, MOp::Nop) {
            continue;
        }
        return matches!(op, MOp::Ret);
    }
    false
}

/// Pass 4g (post-RA only, no jump tables or unwind notes): hoists a distant or post-`Ret` lowered
/// `ParallelCopy` trampoline `S` (`1..=10` non-NOP ops, every non-terminator op is `Mov`/`MovImm`,
/// ending in `MOp::B { target: T }`, no `MNote`s, non-entry, `prev_S` ends with an unconditional
/// terminator):
/// 1. When `prev_T` ends with an unconditional terminator, moves `S` immediately before `T` and
///    folds `S`'s trailing `B T` into zero-branch fallthrough.
/// 2. Otherwise, when `S` sits past the earliest `Ret` block (`s_pos > first_ret_pos > t_pos`),
///    evicts `S` from the cold post-`Ret` tail into the nearest unconditional-terminator boundary
///    in `1..=first_ret_pos` closest to `T` (keeping `B T`), placing hot copy trampolines on the
///    same hot I-cache pages as their targets.
fn mopt_branch_hoist_copy_trampoline_post_ra(
    func: &mut MFunc<'_>,
    entry_blk: MBlock,
    next_in_order: &mut [u32],
    block_ord_pos: &mut [u32],
    stats: &mut MOptStats,
) -> bool {
    if func.is_ssa || func.block_order.len() <= 2 {
        return false;
    }
    let mut hoists: usize = 0;
    let mut moved_any = false;

    while hoists < MAX_TRAMPOLINE_HOISTS {
        let mut did_move = false;
        let n_ord = func.block_order.len();
        let mut first_ret_pos: Option<usize> = None;
        for (pos, &blk) in func.block_order.iter().enumerate() {
            if block_ends_with_ret(func, blk) {
                first_ret_pos = Some(pos);
                break;
            }
        }

        for s_pos in 1..n_ord {
            let Some(&blk_s) = func.block_order.get(s_pos) else {
                continue;
            };
            if blk_s == entry_blk
                || !block_has_no_notes(func, blk_s)
                || !block_has_no_pinned(func, blk_s)
            {
                continue;
            }
            let Some(&prev_s) = func.block_order.get(s_pos - 1) else {
                continue;
            };
            if !block_ends_with_uncond_term(func, prev_s) {
                continue;
            }

            let (start, end) = block_range(func, blk_s);
            let mut non_nop_count: usize = 0;
            let mut last_idx: Option<usize> = None;
            let mut all_copy_prefix = true;
            for i in start..end {
                let op = op_at(func, i);
                if matches!(op, MOp::Nop) {
                    continue;
                }
                if let Some(prev_i) = last_idx {
                    let prev_op = op_at(func, prev_i);
                    if !matches!(prev_op, MOp::Mov { .. } | MOp::MovImm { .. })
                        || is_protected(func, prev_i)
                    {
                        all_copy_prefix = false;
                        break;
                    }
                }
                non_nop_count += 1;
                if non_nop_count > MAX_TRAMPOLINE_HOIST_INSTS {
                    all_copy_prefix = false;
                    break;
                }
                last_idx = Some(i);
            }
            if !all_copy_prefix || non_nop_count == 0 {
                continue;
            }
            let Some(last) = last_idx else {
                continue;
            };
            if is_protected(func, last) {
                continue;
            }
            let MOp::B { target: blk_t } = op_at(func, last) else {
                continue;
            };
            if blk_s == blk_t || blk_t == entry_blk {
                continue;
            }

            let t_pos_u = at(block_ord_pos, to_idx(blk_t.0), u32::MAX);
            if t_pos_u == u32::MAX || t_pos_u == 0 {
                continue;
            }
            let t_pos = to_idx(t_pos_u);
            if t_pos >= n_ord || s_pos <= t_pos {
                continue;
            }

            let past_first_ret =
                first_ret_pos.is_some_and(|rp| t_pos < rp && s_pos > rp);
            let far_after_t = if s_pos > t_pos.saturating_add(MIN_TRAMPOLINE_HOIST_DIST)
                || past_first_ret
            {
                true
            } else {
                let mut past_ret = false;
                for mid_pos in (t_pos + 1)..s_pos {
                    if let Some(&mid_blk) = func.block_order.get(mid_pos)
                        && block_ends_with_ret(func, mid_blk)
                    {
                        past_ret = true;
                        break;
                    }
                }
                past_ret
            };
            if !far_after_t {
                continue;
            }

            let Some(&prev_t) = func.block_order.get(t_pos - 1) else {
                continue;
            };
            if block_ends_with_uncond_term(func, prev_t) {
                // Case 1: Shift `blk_s` immediately before `blk_t` and fold its trailing `B blk_t`
                // into zero-branch fallthrough.
                move_block_slice_before(&mut func.block_order, s_pos, 1, t_pos);
                mark_deleted(func, last);
                stats.branches_folded = stats.branches_folded.saturating_add(1);
                refresh_order_maps(func, next_in_order, block_ord_pos);
                hoists += 1;
                moved_any = true;
                did_move = true;
                break;
            }

            // Case 2: `prev_t` falls through into `blk_t`, but `blk_s` is stranded past the first
            // `Ret` block (`t_pos < first_ret_pos < s_pos`). Evict `blk_s` to the nearest
            // unconditional-terminator boundary in `1..=first_ret_pos` closest to `t_pos`, keeping
            // `B blk_t`. Because `blk_s` moves strictly ahead of `first_ret_pos`, it can never be
            // picked again by Case 2 (strict monotone termination).
            if let Some(ret_pos) = first_ret_pos
                && past_first_ret
            {
                let mut best_ins: Option<(usize, usize)> = None;
                for ins_pos in 1..=ret_pos {
                    if ins_pos == s_pos || ins_pos == s_pos + 1 {
                        continue;
                    }
                    let Some(&prev_ins) = func.block_order.get(ins_pos - 1) else {
                        continue;
                    };
                    if !block_ends_with_uncond_term(func, prev_ins) {
                        continue;
                    }
                    let dist = ins_pos.abs_diff(t_pos);
                    let better = match best_ins {
                        None => true,
                        Some((best_dist, _)) => dist < best_dist,
                    };
                    if better {
                        best_ins = Some((dist, ins_pos));
                    }
                }
                if let Some((_, ins_pos)) = best_ins {
                    move_block_slice_before(&mut func.block_order, s_pos, 1, ins_pos);
                    refresh_order_maps(func, next_in_order, block_ord_pos);
                    hoists += 1;
                    moved_any = true;
                    did_move = true;
                    break;
                }
            }
        }
        if !did_move {
            break;
        }
    }

    moved_any
}

/// Pass 4: threads branches through empty pass-through blocks (`<= 16` hops), inverts
/// `BCond`/`Cbz`/`Cbnz`/`Tbz`/`Tbnz` followed by `B` when the conditional branch targets the next
/// layout block (directly or across empty/pass-through blocks) or when a forward exit precedes a
/// backward loop back-edge, eliminates redundant fallthrough `B`s, prunes unreachable
/// empty/trampoline blocks, merges identical short tail blocks up to 12 ops (Pass 4e), hoists
/// distant / post-`Ret` copy trampolines immediately before fallthrough-free targets (Pass 4g),
/// relocates short edge-split blocks & distant back-edge trampolines (Pass 4d Phases 1–3),
/// relocates short hot loop headers before their fallthrough-free bodies (Pass 4f), compacts NOPs,
/// and rebuilds CFG CSRs.
fn mopt_branch_opt<'a>(func: &mut MFunc<'a>, alloc: Alloc<'a>, stats: &mut MOptStats) {
    let num_blocks = func.block_insts.len();
    let Some(&entry_blk) = func.block_order.first() else {
        return;
    };
    if num_blocks == 0 {
        return;
    }

    let mut next_in_order: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    let mut block_ord_pos: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
    for _ in 0..num_blocks {
        next_in_order.push(u32::MAX);
        block_ord_pos.push(u32::MAX);
    }
    refresh_order_maps(func, &mut next_in_order, &mut block_ord_pos);

    let mut cfg_changed = false;
    let n_order = func.block_order.len();

    // 4a. Thread branches targeting empty pass-through blocks (bounded hops <= 16).
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        for i in start..end {
            if is_protected(func, i) {
                continue;
            }
            if let Some(target) = op_at(func, i).branch_target() {
                let threaded = thread_branch_target(func, &next_in_order, target);
                if threaded != target {
                    if let Some(op_mut) = func.ops.get_mut(i) {
                        set_branch_target(op_mut, threaded);
                    }
                    stats.branches_folded = stats.branches_folded.saturating_add(1);
                    cfg_changed = true;
                }
            }
        }
    }

    // 4b. Conditional-branch inversion over trailing `B` + redundant fallthrough `B` elision.
    if mopt_branch_fold_fallthrough(func, &next_in_order, &block_ord_pos, stats) {
        cfg_changed = true;
    }

    // Compact NOPs and ensure CFG CSRs are current before pruning unreachable empty blocks
    // (pruning first exposes clean `P -> T` boundaries for tail merging and relocation!).
    compact_nops(func, alloc);
    if cfg_changed {
        func.rebuild_cfg(alloc);
    }

    // 4c. Prune unreachable empty / unprotected-trampoline blocks from `func.block_order`
    //     (never dropping `block_order[0]`, never dropping when jump tables / indirect `Br` or
    //     `MNoteKind::Unwind` notes exist, and keeping every target/fallthrough reachable from
    //     any kept block).
    let has_indirect_or_unwind = has_indirect_or_unwind_cfg(func);

    if !has_indirect_or_unwind && func.block_order.len() > 1 {
        let _ = prune_unreachable_empty_blocks(
            func,
            alloc,
            &mut next_in_order,
            &mut block_ord_pos,
            stats,
        );
    }

    // 4e. Post-RA identical short-block tail merging (no jump tables or unwind notes):
    //     deduplicates identical `1..=12`-op tail blocks ending in `B target` (preferring one
    //     immediately preceding `target`), redirects branches, and prunes the unreachable copies.
    if !func.is_ssa
        && !has_indirect_or_unwind
        && mopt_branch_tail_merge_post_ra(func, alloc, &next_in_order, &block_ord_pos, stats)
    {
        compact_nops(func, alloc);
        func.rebuild_cfg(alloc);
        let pruned = prune_unreachable_empty_blocks(
            func,
            alloc,
            &mut next_in_order,
            &mut block_ord_pos,
            stats,
        );
        if !pruned {
            refresh_order_maps(func, &mut next_in_order, &mut block_ord_pos);
            if mopt_branch_fold_fallthrough(func, &next_in_order, &block_ord_pos, stats) {
                compact_nops(func, alloc);
                func.rebuild_cfg(alloc);
            }
        }
    }

    let mut relocated_any = false;

    // 4g. Post-RA hoist distant or post-`Ret` lowered `ParallelCopy` trampolines (`1..=10` non-NOP
    //     ops, every non-terminator op `Mov`/`MovImm`, ending in `B T`) immediately before
    //     fallthrough-free `T`, folding the trailing `B T` into zero-branch fallthrough.
    if !func.is_ssa
        && !has_indirect_or_unwind
        && mopt_branch_hoist_copy_trampoline_post_ra(
            func,
            entry_blk,
            &mut next_in_order,
            &mut block_ord_pos,
            stats,
        )
    {
        relocated_any = true;
    }

    // 4d. Relocate short edge-split blocks (`S -> B T`, `1..=6` non-NOP insts), capped at
    //     `MAX_BLOCK_RELOCATIONS`:
    //     - Phase 1 (direct relocation): move `S` immediately before `T` whenever neither `PrevS`
    //       falls through into `S` nor `P` falls through into `T`, folding `S`'s trailing `B T`
    //       into fallthrough.
    //     - Phase 2 (secondary I-cache co-location): when `P = block_order[t_pos - 1]` is a short
    //       pure fallthrough copy block (`1..=4` non-NOP insts, no branch/terminator) preceded by
    //       an unconditional terminator `PP = block_order[t_pos - 2]` and `S` sits after `T`
    //       (`s_pos > t_pos`), move `S` to `t_pos - 1` (immediately before `P`) while keeping
    //       `S`'s `B T` so `S` shares `T`'s I-cache line instead of jumping from the cold tail.
    //     - Phase 3 (post-RA distant back-edge trampoline inversion & co-location): when `P` ends
    //       with a single invertible conditional branch targeting a single-pred `1..=3`-op back-edge
    //       trampoline `S -> B T` (`pos[T] <= p_pos`) whose predecessor in `block_order` ends with
    //       an unconditional terminator, invert `P`'s branch to its fallthrough `F` and move `S`
    //       immediately after `P`.
    let mut relocations: usize = 0;
    for allow_secondary in [false, true] {
        while relocations < MAX_BLOCK_RELOCATIONS {
            let mut did_move = false;
            let n_ord = func.block_order.len();
            for s_pos in 1..n_ord {
                let Some(&blk_s) = func.block_order.get(s_pos) else {
                    continue;
                };
                let Some(&prev_s) = func.block_order.get(s_pos - 1) else {
                    continue;
                };
                if !block_ends_with_uncond_term(func, prev_s) {
                    continue;
                }
                let (start, end) = block_range(func, blk_s);
                let mut non_nop_count: usize = 0;
                let mut last_idx: Option<usize> = None;
                for i in start..end {
                    if !matches!(op_at(func, i), MOp::Nop) {
                        non_nop_count += 1;
                        last_idx = Some(i);
                    }
                }
                if non_nop_count == 0 || non_nop_count > MAX_RELOCATE_BLOCK_INSTS {
                    continue;
                }
                let Some(last) = last_idx else {
                    continue;
                };
                if is_protected(func, last) {
                    continue;
                }
                let MOp::B { target: blk_t } = op_at(func, last) else {
                    continue;
                };
                if blk_t == blk_s || blk_t == entry_blk {
                    continue;
                }
                let t_pos_u = block_ord_pos.get(to_idx(blk_t.0)).copied().unwrap_or(u32::MAX);
                if t_pos_u == u32::MAX || t_pos_u == 0 {
                    continue;
                }
                let t_pos = to_idx(t_pos_u);
                if t_pos >= n_ord || s_pos + 1 == t_pos {
                    continue;
                }
                let Some(&blk_p) = func.block_order.get(t_pos - 1) else {
                    continue;
                };
                if !allow_secondary {
                    if !block_ends_with_uncond_term(func, blk_p) {
                        continue;
                    }

                    // Shift `blk_s` in `func.block_order` to immediately precede `blk_t` (zero `Vec::remove`).
                    move_block_slice_before(&mut func.block_order, s_pos, 1, t_pos);

                    // Immediately delete `blk_s`'s trailing `B blk_t` so `blk_s` falls through into `blk_t`
                    // (and is no longer an uncond-terminated separator before `blk_t`).
                    mark_deleted(func, last);
                    stats.branches_folded = stats.branches_folded.saturating_add(1);
                    refresh_order_maps(func, &mut next_in_order, &mut block_ord_pos);
                    relocations += 1;
                    relocated_any = true;
                    did_move = true;
                    break;
                }

                // Secondary I-cache co-location: `P` falls through into `T` as a short pure copy
                // block (`1..=4` non-NOP insts, no branch/terminator), `PP` ends with an
                // unconditional terminator, and `S` sits after `T` (`s_pos > t_pos`).
                if block_ends_with_uncond_term(func, blk_p)
                    || t_pos < 2
                    || s_pos <= t_pos
                    || blk_p == blk_s
                    || blk_p == entry_blk
                    || !is_short_pure_fallthrough_block(func, blk_p)
                {
                    continue;
                }
                let Some(&blk_pp) = func.block_order.get(t_pos - 2) else {
                    continue;
                };
                if !block_ends_with_uncond_term(func, blk_pp) {
                    continue;
                }

                // Shift `blk_s` to `t_pos - 1` (immediately before `blk_p`) while keeping `B blk_t`.
                // Because `s_pos > t_pos`, `s_pos` strictly decreases to `t_pos - 1 < t_pos`,
                // so `blk_s` can never ping-pong on subsequent iterations.
                move_block_slice_before(&mut func.block_order, s_pos, 1, t_pos - 1);

                refresh_order_maps(func, &mut next_in_order, &mut block_ord_pos);
                relocations += 1;
                relocated_any = true;
                did_move = true;
                break;
            }
            if !did_move {
                break;
            }
        }
    }

    // Pass 4d Phase 3: post-RA invert & co-locate distant back-edge trampolines.
    if !func.is_ssa
        && !has_indirect_or_unwind
        && func.block_order.len() > 2
        && relocations < MAX_BLOCK_RELOCATIONS
    {
        if relocated_any || func.block_preds_off.len() != num_blocks + 1 {
            compact_nops(func, alloc);
            func.rebuild_cfg(alloc);
            refresh_order_maps(func, &mut next_in_order, &mut block_ord_pos);
        }

        let mut inverted_p: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(num_blocks, alloc);
        for _ in 0..num_blocks {
            inverted_p.push(false);
        }

        while relocations < MAX_BLOCK_RELOCATIONS {
            let mut did_move = false;
            let n_ord = func.block_order.len();
            for p_pos in 0..n_ord.saturating_sub(1) {
                let Some(&blk_p) = func.block_order.get(p_pos) else {
                    continue;
                };
                if at(&inverted_p, to_idx(blk_p.0), true) {
                    continue;
                }
                let Some(&blk_f) = func.block_order.get(p_pos + 1) else {
                    continue;
                };
                let (p_start, p_end) = block_range(func, blk_p);
                let mut last_idx: Option<usize> = None;
                let mut prev_idx: Option<usize> = None;
                for i in (p_start..p_end).rev() {
                    if matches!(op_at(func, i), MOp::Nop) {
                        continue;
                    }
                    if last_idx.is_none() {
                        last_idx = Some(i);
                    } else {
                        prev_idx = Some(i);
                        break;
                    }
                }
                let Some(p_last) = last_idx else {
                    continue;
                };
                if is_protected(func, p_last) {
                    continue;
                }
                let op_last = op_at(func, p_last);
                if !op_last.is_conditional_branch() {
                    continue;
                }
                if let Some(p_prev) = prev_idx {
                    let op_prev = op_at(func, p_prev);
                    if op_prev.is_conditional_branch() || op_prev.is_unconditional_terminator() {
                        continue;
                    }
                }
                let Some(blk_s) = op_last.branch_target() else {
                    continue;
                };
                let Some(inv_op) = invert_cond_branch(op_last, blk_f) else {
                    continue;
                };
                if blk_s == blk_p || blk_s == blk_f || blk_s == entry_blk {
                    continue;
                }
                let s_pos_u = at(&block_ord_pos, to_idx(blk_s.0), u32::MAX);
                if s_pos_u == u32::MAX || s_pos_u == 0 {
                    continue;
                }
                let s_pos = to_idx(s_pos_u);
                if s_pos >= n_ord || s_pos == p_pos + 1 {
                    continue;
                }
                if func.block_preds(blk_s) != [blk_p].as_slice() {
                    continue;
                }
                let Some(&prev_s) = func.block_order.get(s_pos - 1) else {
                    continue;
                };
                if !block_ends_with_uncond_term(func, prev_s) {
                    continue;
                }
                let Some(blk_t) = is_short_backedge_trampoline(func, &block_ord_pos, blk_s, p_pos)
                else {
                    continue;
                };
                if blk_f == blk_t
                    || is_short_backedge_trampoline(func, &block_ord_pos, blk_f, p_pos).is_some()
                {
                    continue;
                }

                set(&mut func.ops, p_last, inv_op);
                move_block_slice_before(&mut func.block_order, s_pos, 1, p_pos + 1);
                set(&mut inverted_p, to_idx(blk_p.0), true);
                stats.branches_folded = stats.branches_folded.saturating_add(1);
                refresh_order_maps(func, &mut next_in_order, &mut block_ord_pos);
                relocations += 1;
                relocated_any = true;
                did_move = true;
                break;
            }
            if !did_move {
                break;
            }
        }
    }

    // 4f. Post-RA relocate short displaced hot loop-header `H` (or `[prev_H, H]`) immediately
    //     before its fallthrough-free `Body` and invert `b.cc Body; b Exit` -> `b.!cc Exit`.
    if !func.is_ssa
        && !has_indirect_or_unwind
        && mopt_branch_relocate_hot_loop_header_post_ra(
            func,
            entry_blk,
            &mut next_in_order,
            &mut block_ord_pos,
            stats,
        )
    {
        relocated_any = true;
    }

    if relocated_any {
        let _ = mopt_branch_fold_fallthrough(func, &next_in_order, &block_ord_pos, stats);
        compact_nops(func, alloc);
        func.rebuild_cfg(alloc);
    }
}

// -----------------------------------------------------------------------------
// Pass 5 (Post-RA Only): Apple Silicon Macro-Op Fusion (`add/sub + cbz/cbnz`)
// -----------------------------------------------------------------------------

/// Post-RA (`!func.is_ssa`) Apple Silicon macro-op fusion: rewrites adjacent unpinned, noteless
/// `SubImm`/`AddImm`/`SubReg`/`AddReg` (`sets_flags: false`, GPR `dst`, non-`SP` `rn`) +
/// `Cbz`/`Cbnz` (same register & width) into `sets_flags: true` + `BCond(Eq/Ne)` with a fresh
/// virtual flag whenever `NZCV` is dead across the pair.
fn mopt_fuse_addsub_cbz_post_ra(func: &mut MFunc<'_>, stats: &mut MOptStats) {
    if func.is_ssa {
        return;
    }

    let mut max_vflag: Option<u32> = None;
    for &vf in func.vflag_def.iter().chain(func.vflag_use.iter()) {
        if vf != NO_VFLAG {
            max_vflag = Some(match max_vflag {
                Some(cur) if cur >= vf => cur,
                _ => vf,
            });
        }
    }
    let mut next_vflag: u32 = match max_vflag {
        Some(m) => match m.checked_add(1) {
            Some(v) if v != NO_VFLAG => v,
            _ => return,
        },
        None => 0,
    };

    let n_order = func.block_order.len();
    for bi in 0..n_order {
        let Some(&blk) = func.block_order.get(bi) else {
            continue;
        };
        let (start, end) = block_range(func, blk);
        if end.saturating_sub(start) < 2 {
            continue;
        }

        for i in start..(end - 1) {
            let next = i + 1;
            if is_protected(func, i)
                || is_protected(func, next)
                || vflag_def_at(func, i) != NO_VFLAG
                || vflag_use_at(func, i) != NO_VFLAG
                || vflag_def_at(func, next) != NO_VFLAG
                || vflag_use_at(func, next) != NO_VFLAG
            {
                continue;
            }

            let (dst, wide) = match op_at(func, i) {
                MOp::SubImm { dst, rn, wide, sets_flags: false, .. }
                | MOp::AddImm { dst, rn, wide, sets_flags: false, .. }
                | MOp::SubReg { dst, rn, wide, sets_flags: false, .. }
                | MOp::AddReg { dst, rn, wide, sets_flags: false, .. }
                    if matches!(dst, MReg::P(PReg::Gpr(r)) if r <= 30) && rn != MReg::SP =>
                {
                    (dst, wide)
                }
                _ => continue,
            };

            let (cond, target) = match op_at(func, next) {
                MOp::Cbz { rt, target, wide: w2 } if rt == dst && w2 == wide => (MCond::Eq, target),
                MOp::Cbnz { rt, target, wide: w2 } if rt == dst && w2 == wide => {
                    (MCond::Ne, target)
                }
                _ => continue,
            };

            // Verify NZCV is dead after `next`: no subsequent instruction in the block may read
            // flags (either via `vflag_use` or architectural `reads_flags()`), and if a `Ret`
            // follows without an intervening flag write, ensure no prior flag write is unconsumed.
            let mut flag_live_after = false;
            let mut saw_ret_after = false;
            for k in (next + 1)..end {
                let op_k = op_at(func, k);
                if matches!(op_k, MOp::Nop) {
                    continue;
                }
                if vflag_use_at(func, k) != NO_VFLAG || op_k.reads_flags() {
                    flag_live_after = true;
                    break;
                }
                if matches!(op_k, MOp::Ret) {
                    saw_ret_after = true;
                }
                if op_k.writes_flags() {
                    break;
                }
            }
            if flag_live_after {
                continue;
            }
            if saw_ret_after {
                let mut unconsumed_flag_before = false;
                for k in start..i {
                    let op_k = op_at(func, k);
                    if matches!(op_k, MOp::Nop) {
                        continue;
                    }
                    if op_k.reads_flags() {
                        unconsumed_flag_before = false;
                    }
                    if op_k.writes_flags() {
                        unconsumed_flag_before = true;
                    }
                }
                if unconsumed_flag_before {
                    continue;
                }
            }

            if next_vflag == NO_VFLAG {
                return;
            }
            let vf = next_vflag;
            next_vflag = match next_vflag.checked_add(1) {
                Some(v) => v,
                None => NO_VFLAG,
            };

            if let Some(op_i_mut) = func.ops.get_mut(i) {
                match op_i_mut {
                    MOp::SubImm { sets_flags, .. }
                    | MOp::AddImm { sets_flags, .. }
                    | MOp::SubReg { sets_flags, .. }
                    | MOp::AddReg { sets_flags, .. } => {
                        *sets_flags = true;
                    }
                    _ => {}
                }
            }
            set(&mut func.vflag_def, i, vf);
            set(&mut func.ops, next, MOp::BCond { cond, target });
            set(&mut func.vflag_use, next, vf);
            stats.peepholes = stats.peepholes.saturating_add(1);
        }
    }
}

// -----------------------------------------------------------------------------
// Single-Pass O(N) NOP Compaction (preserving all `MNote`s and `block_insts`)
// -----------------------------------------------------------------------------

/// Removes all non-noted, non-pinned `MOp::Nop` instructions in one O(N) sweep, updating all
/// lockstep SoA columns (`ops`, `flags`, `vflag_def`, `vflag_use`, `note_idx`) and every
/// `(start, end)` range in `func.block_insts`.
fn compact_nops<'a>(func: &mut MFunc<'a>, alloc: Alloc<'a>) {
    let old_len = func.ops.len();
    let mut has_removable_nop = false;
    for i in 0..old_len {
        if matches!(op_at(func, i), MOp::Nop)
            && note_at(func, i) == NO_NOTE
            && !flags_at(func, i).contains(MInstFlags::PINNED)
        {
            has_removable_nop = true;
            break;
        }
    }
    if !has_removable_nop {
        return;
    }

    let mut old_to_new: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(old_len.saturating_add(1), alloc);
    let mut dst: usize = 0;
    for src in 0..old_len {
        old_to_new.push(u32_of(dst).unwrap_or(u32::MAX));
        let keep = !matches!(op_at(func, src), MOp::Nop)
            || note_at(func, src) != NO_NOTE
            || flags_at(func, src).contains(MInstFlags::PINNED);
        if keep {
            if dst != src {
                copy_inst_slot(func, dst, src);
            }
            dst += 1;
        }
    }
    old_to_new.push(u32_of(dst).unwrap_or(u32::MAX));

    func.ops.truncate(dst);
    func.flags.truncate(dst);
    func.vflag_def.truncate(dst);
    func.vflag_use.truncate(dst);
    func.note_idx.truncate(dst);

    for (s, e) in func.block_insts.iter_mut() {
        let si = to_idx(*s);
        let ei = to_idx(*e);
        if let Some(&new_s) = old_to_new.get(si) {
            *s = new_s;
        }
        if let Some(&new_e) = old_to_new.get(ei) {
            *e = new_e;
        }
    }
}

// -----------------------------------------------------------------------------
// Unit & Apple Silicon Hardware Execution Tests (NO `Global` / NO `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    use blaze_own::Vec;
    use blaze_tier::Os;

    use super::{MOptStats, mov_imm_inst_cost, optimize_mfunc};
    use crate::sota::mfunc::{
        AddrMode, DEFAULT_RET_LIVE_GPRS, LogicalKind, MCond, MFunc, MFuncOpts, MInstFlags,
        MNoteKind, MOp, MReg, MShift, MemSize, NO_VFLAG, PReg, ShiftKind, VRegClass, mencode,
        mverify,
    };

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn new_post_ra() -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os: Os::MacOs,
            is_ssa: false,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    fn new_ssa() -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os: Os::MacOs,
            is_ssa: true,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn words_to_bytes(words: &[u32], alloc: Alloc<'static>) -> Vec<u8, Alloc<'static>> {
        let mut bytes: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(words.len() * 4, alloc);
        for &w in words {
            for b in w.to_le_bytes() {
                bytes.push(b);
            }
        }
        bytes
    }

    #[test]
    fn mopt_sub_into_cmp_fuses_distinct_and_rejects_self_clobber() {
        let alloc = test_alloc();

        // 1. SSA `VReg` positive control: `v1 = sub v0, #8; cmp v0, #8; csel v2, v1, v0, hs`
        //    -> fuses into single `subs v1, v0, #8` preserving `vflag_def = 0`!
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let sb0 = ssa.new_block(false);
        ssa.begin_block(sb0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 20, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::SubImm {
                dst: MReg::V(v1),
                rn: MReg::V(v0),
                imm12: 8,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let s_cmp = ssa.push_inst(
            MOp::CmpImm { rn: MReg::V(v0), imm12: 8, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(s_cmp, 0);
        let s_sel = ssa.push_inst(
            MOp::Csel {
                dst: MReg::V(v2),
                rn: MReg::V(v1),
                rm: MReg::V(v0),
                cond: MCond::Hs,
                wide: true,
            },
            MInstFlags::NONE,
        );
        ssa.set_vflag_use(s_sel, 0);
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v2), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(sb0);

        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats: MOptStats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.peepholes >= 1, "SSA SubImm+CmpImm must fuse");
        assert!(
            ssa.ops.iter().any(|op| matches!(
                *op,
                MOp::SubImm { dst, rn, imm12: 8, shift12: false, wide: true, sets_flags: true }
                    if dst == MReg::V(v1) && rn == MReg::V(v0)
            )),
            "expected fused flag-setting SubImm in SSA"
        );
        assert!(
            !ssa.ops.iter().any(|op| matches!(*op, MOp::CmpImm { .. })),
            "CmpImm must be eliminated after SSA SubImm+CmpImm fusion"
        );

        // 2. Post-RA positive control (`dst != rn`):
        //    `sub x1, x0, #8; cmp x0, #8; csel x0, x1, x0, hs; ret`
        //    Computes `if x0 >= 8 { x0 - 8 } else { x0 }`.
        let mut pos = new_post_ra();
        let pb0 = pos.new_block(false);
        pos.begin_block(pb0);
        pos.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X0),
                imm12: 8,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let p_cmp = pos.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 8, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        pos.set_vflag_def(p_cmp, 0);
        let p_sel = pos.push_inst(
            MOp::Csel {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X0),
                cond: MCond::Hs,
                wide: true,
            },
            MInstFlags::NONE,
        );
        pos.set_vflag_use(p_sel, 0);
        pos.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        pos.end_block(pb0);

        assert_eq!(mverify(&pos), Ok(()));
        let pos_stats = optimize_mfunc(&mut pos, alloc);
        assert_eq!(mverify(&pos), Ok(()));
        assert_eq!(pos_stats.peepholes, 1);
        assert_eq!(pos.ops.len(), 3, "fused SubImm + Csel + Ret = 3 ops");
        let pos_enc = mencode(&pos, alloc).expect("mencode positive post-RA must succeed");

        // 3. Post-RA NEGATIVE control (`dst == rn`, self-clobber MUST NOT fuse!):
        //    `sub x0, x0, #8; cmp x0, #8; csel x0, x0, xzr, hs; ret`
        //    For x0 = 10: x0 becomes 2, then `cmp 2, 8` is Lo -> returns 0!
        //    (Wrong fusion `subs x0, x0, #8` would compare 10 >= 8 -> Hs -> returns 2!)
        let mut neg = new_post_ra();
        let nb0 = neg.new_block(false);
        neg.begin_block(nb0);
        neg.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 8,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let n_cmp = neg.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 8, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        neg.set_vflag_def(n_cmp, 0);
        let n_sel = neg.push_inst(
            MOp::Csel {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::XZR,
                cond: MCond::Hs,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg.set_vflag_use(n_sel, 0);
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb0);

        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.peepholes, 0, "self-clobber rd == rn MUST NOT fuse");
        assert_eq!(neg.ops.len(), 4, "SubImm + CmpImm + Csel + Ret must remain 4 ops");
        let neg_enc = mencode(&neg, alloc).expect("mencode negative post-RA must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`/`X1`.
            let f_pos: extern "C" fn(u64) -> u64 = unsafe { pos_exec.as_fn() };
            for &x in &[0u64, 7, 8, 9, 100] {
                let expected = if x >= 8 { x - 8 } else { x };
                assert_eq!(f_pos(x), expected);
            }

            let neg_bytes = words_to_bytes(&neg_enc.words, alloc);
            let neg_exec =
                blaze_inproc::exec::Executable::new(&neg_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`.
            let f_neg: extern "C" fn(u64) -> u64 = unsafe { neg_exec.as_fn() };
            assert_eq!(f_neg(10), 0, "x0=10: (10-8)=2 < 8 -> must return 0");
            assert_eq!(f_neg(15), 0, "x0=15: (15-8)=7 < 8 -> must return 0");
            assert_eq!(f_neg(16), 8, "x0=16: (16-8)=8 >= 8 -> must return 8");
            assert_eq!(f_neg(20), 12, "x0=20: (20-8)=12 >= 8 -> must return 12");
        }
        let _ = (pos_enc, neg_enc);
    }

    #[test]
    fn mopt_redundant_load_and_store_forwarding_respects_alias_and_volatile() {
        let alloc = test_alloc();
        // Signature: `fn(ptr: *mut u64, other: *mut u64, val: u64) -> u64` (`X0`, `X1`, `X2`).
        let mut func = new_post_ra();
        let b0 = func.new_block(false);
        func.begin_block(b0);

        // 1. Store `val` to `[x0, #0]`, then reload twice -> both forward to `mov` from `x2`!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X2),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 0 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X3),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X4),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );

        // 2. Load `[x0, #8]` twice -> second is redundant and forwarded to `mov x6, x5`!
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X5),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X6),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );

        // 3. Potentially aliasing store via different base `x1`: MUST invalidate `[x0, #8]`!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X2),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 8 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        // Reload `[x0, #8]` AFTER aliasing store -> MUST remain an actual `Ldr`!
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X7),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );

        // 4. Volatile load of `[x0, #0]` -> MUST remain an actual `Ldr` (never forwarded)!
        let i_vol = func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X9),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ.union(MInstFlags::VOLATILE),
        );
        func.attach_note(i_vol, MNoteKind::Volatile);

        // Sum: x0 = x3 + x4 + x5 + x6 + x7 + x9; ret.
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X3),
                rm: MReg::p(PReg::X4),
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
                rm: MReg::p(PReg::X5),
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
                rm: MReg::p(PReg::X6),
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
                rm: MReg::p(PReg::X7),
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
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats.loads_forwarded, 3, "expected 2 store-forwards + 1 redundant load");

        // Surviving loads must be exactly 3: initial `[x0, #8]`, post-alias `[x0, #8]`, volatile `[x0, #0]`.
        let mut ldr_count: usize = 0;
        let mut saw_volatile_ldr = false;
        for (idx, op) in func.ops.iter().enumerate() {
            if matches!(*op, MOp::Ldr { .. }) {
                ldr_count += 1;
                if func.flags[idx].contains(MInstFlags::VOLATILE) {
                    saw_volatile_ldr = true;
                }
            }
        }
        assert_eq!(ldr_count, 3, "post-alias and volatile loads must survive");
        assert!(saw_volatile_ldr, "volatile load must keep its VOLATILE flag");

        let enc_out = mencode(&func, alloc).expect("mencode must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function reading/writing valid 2-element `*mut u64` buffers.
            let f: extern "C" fn(*mut u64, *mut u64, u64) -> u64 = unsafe { exec.as_fn() };

            // Case A: Aliased buffers (`other == ptr`): `[ptr+8]` changes from 100 to 7!
            // Expected: x3(7) + x4(7) + x5(100) + x6(100) + x7(7) + x9(7) = 228.
            let mut aliased = [1u64, 100u64];
            let res_alias = f(aliased.as_mut_ptr(), aliased.as_mut_ptr(), 7);
            assert_eq!(res_alias, 228, "aliasing store via x1 must invalidate [x0, #8]");
            assert_eq!(aliased, [7, 7]);

            // Case B: Disjoint buffers (`other != ptr`): `[ptr+8]` stays 100!
            // Expected: x3(7) + x4(7) + x5(100) + x6(100) + x7(100) + x9(7) = 321.
            let mut buf_a = [1u64, 100u64];
            let mut buf_b = [0u64, 0u64];
            let res_disjoint = f(buf_a.as_mut_ptr(), buf_b.as_mut_ptr(), 7);
            assert_eq!(res_disjoint, 321);
            assert_eq!(buf_a, [7, 100]);
            assert_eq!(buf_b, [0, 7]);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_pairs_ldp_stp_and_fuses_post_index() {
        let alloc = test_alloc();
        // Signature: `fn(src: *const u64, dst: *mut u64, pairs: u64) -> u64` (`X0`, `X1`, `X2`).
        // Copies `pairs` adjacent `u64` pairs from `src` to `dst` and returns their total sum.
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X5), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        // Dead self-mov to verify self-mov elimination alongside pairing.
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X5), src: MReg::p(PReg::X5), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X2), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        func.begin_block(b_loop);
        // Two adjacent loads + pointer increment -> `ldp x3, x4, [x0], #16`!
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X3),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X4),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 16,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // Two adjacent stores + pointer increment -> `stp x3, x4, [x1], #16`!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X3),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 0 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X4),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 8 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                imm12: 16,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // Accumulate sum & loop latch (`sub x2, x2, #1; cmp x2, #0; b.ne b_loop` -> flag fusion!).
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X5),
                rm: MReg::p(PReg::X3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X5),
                rm: MReg::p(PReg::X4),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X2), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let br = func.push_inst(
            MOp::BCond { cond: MCond::Ne, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br, 0);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X5), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));

        assert_eq!(stats.movs_eliminated, 1, "self-mov x5, x5 must be eliminated");
        assert_eq!(stats.peepholes, 1, "sub x2, #1 + cmp x2, #0 must fuse into subs");
        assert_eq!(stats.pairs_formed, 2, "must form 1 Ldp and 1 Stp");
        assert_eq!(stats.post_idx_formed, 2, "must fuse both Ldp and Stp with +16 post-index");

        assert!(func.ops.iter().any(|op| matches!(
            *op,
            MOp::Ldp {
                dst0: MReg::P(PReg::X3),
                dst1: MReg::P(PReg::X4),
                base: MReg::P(PReg::X0),
                off: 16,
                size: MemSize::X64,
                post_idx: true,
                pre_idx: false,
            }
        )));
        assert!(func.ops.iter().any(|op| matches!(
            *op,
            MOp::Stp {
                src0: MReg::P(PReg::X3),
                src1: MReg::P(PReg::X4),
                base: MReg::P(PReg::X1),
                off: 16,
                size: MemSize::X64,
                post_idx: true,
                pre_idx: false,
            }
        )));

        let enc_out = mencode(&func, alloc).expect("mencode must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X5`.
            let f: extern "C" fn(*const u64, *mut u64, u64) -> u64 = unsafe { exec.as_fn() };

            let src = [10u64, 20, 30, 40, 50, 60, 70, 80];
            let mut dst = [0u64; 8];
            let total = f(src.as_ptr(), dst.as_mut_ptr(), 4);
            assert_eq!(total, 360);
            assert_eq!(dst, src);
            assert_eq!(f(src.as_ptr(), dst.as_mut_ptr(), 0), 0);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_branch_opt_inverts_fallthrough_and_threads_trampoline() {
        let alloc = test_alloc();
        // 4 blocks in layout order: `b_entry`, `b_then`, `b_tramp`, `b_done`.
        //   b_entry: cmp x0, #0; b.eq b_then; b b_tramp
        //   b_then:  mov x0, #100; b b_done
        //   b_tramp: b b_done             (empty pass-through trampoline!)
        //   b_done:  add x0, x0, #7; ret
        // Expected after Pass 4:
        //   - `b b_tramp` threads to `b_done`,
        //   - `b.eq b_then; b b_done` inverts to `b.ne b_done` (deleting trailing `B`!),
        //   - `b_tramp`'s fallthrough `b b_done` is eliminated!
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_then = func.new_block(false);
        let b_tramp = func.new_block(false);
        let b_done = func.new_block(false);

        func.begin_block(b_entry);
        let cmp = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let beq = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_then },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(beq, 0);
        func.push_inst(MOp::B { target: b_tramp }, MInstFlags::TERMINATOR);
        func.end_block(b_entry);

        func.begin_block(b_then);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 100, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_done }, MInstFlags::TERMINATOR);
        func.end_block(b_then);

        func.begin_block(b_tramp);
        func.push_inst(MOp::B { target: b_done }, MInstFlags::TERMINATOR);
        func.end_block(b_tramp);

        func.begin_block(b_done);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 7,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_done);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.branches_folded >= 3, "expected trampoline thread + invert + fallthrough elide");

        // Verify `b_entry` fused `cmp x0, #0; b.eq b_then; b b_done` into single `cbnz x0, b_done`!
        let entry_range = func.block_inst_range(b_entry);
        assert_eq!(entry_range.len(), 1);
        assert_eq!(
            func.ops[entry_range.start],
            MOp::Cbnz { rt: MReg::p(PReg::X0), target: b_done, wide: true }
        );
        // Verify `b_tramp` had its fallthrough `B` eliminated.
        assert_eq!(func.block_inst_range(b_tramp).len(), 0);
        // Verify rebuilt CFG successors of `b_entry` are `b_done` and `b_then`.
        assert_eq!(func.block_succs(b_entry), &[b_done, b_then]);

        let enc_out = mencode(&func, alloc).expect("mencode must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(0), 107);
            assert_eq!(f(5), 12);
            assert_eq!(f(42), 49);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_peepholes_ubfm_cinc_and_movimm_cse() {
        let alloc = test_alloc();

        // 1. `Ubfm` LSR + UBFX fusion + `MovImm` CSE + `SubImm #1` + `Csinc` + `CmpImm #0`:
        //    Computes:
        //      u = (x0 >> 8) & 0xfff  (via `lsr x1, x0, #8; ubfx x1, x1, #4, #12` -> `(x0 >> 12) & 0xfff`)
        //      k1 = 0x1234_5678_9abc; k2 = 0x1234_5678_9abc (CSE -> `mov` + copy prop!)
        //      cmp u, #0; sub x2, x0, #1; cinc x2, x2, eq; cmp x2, #0; csel x0, k1, k2_plus_u, eq
        let mut func = new_post_ra();
        let b0 = func.new_block(false);
        func.begin_block(b0);

        // `lsr x1, x0, #8` (`immr = 8, imms = 63`) + `ubfx x1, x1, #4, #12` (`immr = 4, imms = 15`)
        // -> fuses into `ubfx x1, x0, #12, #12` (`immr = 12, imms = 23`)!
        func.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X1), rn: MReg::p(PReg::X0), immr: 8, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X1), rn: MReg::p(PReg::X1), immr: 4, imms: 15, wide: true },
            MInstFlags::NONE,
        );

        // Duplicate multi-word `MovImm` into `x3` and `x4` -> CSE rewrites `x4` to `mov x4, x3`!
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X3), imm: 0x0012_3456_789a, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X4), imm: 0x0012_3456_789a, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );

        // `cmp x1, #0` (vflag 0), then `sub x2, x0, #1; csinc x2, x2, x2, ne (cinc eq); cmp x2, #0` (vflag 1)
        // -> commutes to `csinc x2, x0, x0, ne; subs x2, x2, #1` and deletes `cmp x2, #0`!
        let c0 = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X1), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c0, 0);
        func.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cinc = func.push_inst(
            MOp::Csinc {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X2),
                cond: MCond::Ne,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.set_vflag_use(cinc, 0);
        let c1 = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X2), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c1, 1);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X4),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let sel = func.push_inst(
            MOp::Csel {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X3),
                rm: MReg::p(PReg::X5),
                cond: MCond::Eq,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.set_vflag_use(sel, 1);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.peepholes >= 3, "expected Ubfm fusion + MovImm CSE + Sub1/Csinc commute");
        assert!(func.ops.iter().any(|op| matches!(
            *op,
            MOp::Ubfm {
                dst: MReg::P(PReg::X1),
                rn: MReg::P(PReg::X0),
                immr: 12,
                imms: 23,
                wide: true,
            }
        )));

        let enc_out = mencode(&func, alloc).expect("mencode must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X5`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            let k: u64 = 0x0012_3456_789a;
            // x0 = 0: u = 0 (eq -> cinc adds 1), x2 = (0 - 1) + 1 = 0 -> eq -> returns k
            assert_eq!(f(0), k);
            // x0 = 1: u = 0 (eq -> cinc adds 1), x2 = (1 - 1) + 1 = 1 != 0 -> returns k + 0
            assert_eq!(f(1), k);
            // x0 = 0x5000: u = 5 (!= 0 -> cinc adds 0), x2 = 0x4fff != 0 -> returns k + 5
            assert_eq!(f(0x5000), k + 5);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_known_zero_upper32_elides_redundant_mov_w_and_guards_unknown() {
        let alloc = test_alloc();

        // 1. Post-RA positive control: all 5 sources provably zero bits 32..63, so every
        //    subsequent `mov wR, wR` is elided (also exercises `MOp::Prfm` preservation).
        let mut pos = new_post_ra();
        let pb0 = pos.new_block(false);
        pos.begin_block(pb0);
        pos.push_inst(MOp::Prfm { base: MReg::p(PReg::X1), imm12: 0 }, MInstFlags::MEM_READ);
        // `lsr x2, x0, #32; mov w2, w2`
        pos.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X2), rn: MReg::p(PReg::X0), immr: 32, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X2), src: MReg::p(PReg::X2), wide: false },
            MInstFlags::NONE,
        );
        // `ubfx x3, x0, #4, #16; mov w3, w3`
        pos.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X3), rn: MReg::p(PReg::X0), immr: 4, imms: 19, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X3), src: MReg::p(PReg::X3), wide: false },
            MInstFlags::NONE,
        );
        // `and x4, x0, #0xff; mov w4, w4`
        pos.push_inst(
            MOp::LogicalImm {
                dst: MReg::p(PReg::X4),
                rn: MReg::p(PReg::X0),
                imm: 0xff,
                kind: LogicalKind::And,
                wide: true,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X4), src: MReg::p(PReg::X4), wide: false },
            MInstFlags::NONE,
        );
        // `mov w5, #123; mov w5, w5`
        pos.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X5), imm: 123, wide: false },
            MInstFlags::REMATERIALIZABLE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X5), src: MReg::p(PReg::X5), wide: false },
            MInstFlags::NONE,
        );
        // `ldr w6, [x1, #0]; mov w6, w6`
        pos.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X6),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 0 },
                size: MemSize::W32,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X6), src: MReg::p(PReg::X6), wide: false },
            MInstFlags::NONE,
        );
        // `x0 = x2 + x3 + x4 + x5 + x6; ret`
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X4),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X5),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X6),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        pos.end_block(pb0);

        assert_eq!(mverify(&pos), Ok(()));
        let pos_stats = optimize_mfunc(&mut pos, alloc);
        assert_eq!(mverify(&pos), Ok(()));
        assert_eq!(pos_stats.movs_eliminated, 5, "all 5 redundant `mov wR, wR` must be elided");
        assert!(
            !pos.ops.iter().any(|op| matches!(*op, MOp::Mov { wide: false, .. })),
            "no narrow self-mov should survive"
        );
        assert!(
            pos.ops.iter().any(|op| matches!(*op, MOp::Prfm { .. })),
            "prefetch must be preserved"
        );
        let pos_enc = mencode(&pos, alloc).expect("mencode positive zero-upper-32 must succeed");

        // 2. Pre-RA SSA positive control: `v1 = lsr v0, #32; v2 = mov w_v1; v3 = add x_v2, x_v0`
        //    -> `v2 = mov w_v1` promotes to wide and is copy-propagated & eliminated even into a
        //    64-bit consumer!
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v2 = ssa.alloc_vreg(VRegClass::Gpr32);
        let v3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let sb0 = ssa.new_block(false);
        ssa.begin_block(sb0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 0x0000_0003_0000_0001u64, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::Ubfm { dst: MReg::V(v1), rn: MReg::V(v0), immr: 32, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v2), src: MReg::V(v1), wide: false },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v3),
                rn: MReg::V(v2),
                rm: MReg::V(v0),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v3), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(sb0);

        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.movs_eliminated >= 1, "SSA zero-upper-32 narrow mov must be eliminated");
        assert!(
            !ssa.ops.iter().any(|op| matches!(*op, MOp::Mov { dst, .. } if dst == MReg::V(v2))),
            "SSA v2 narrow mov must be removed"
        );
        // Either `AddReg(v1, v0)` (after mov elision) or the even tighter fused
        // `AddReg(v0, v0, lsr #32)` (after shift-into-ALU fusion chains on top!) is valid.
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg { dst, rn, rm, wide: true, shift: None, .. }
                if dst == MReg::V(v3) && rn == MReg::V(v1) && rm == MReg::V(v0)
        ) || matches!(
            *op,
            MOp::AddReg {
                dst,
                rn,
                rm,
                wide: true,
                shift: Some(MShift::Lsr(32)),
                ..
            } if dst == MReg::V(v3) && rn == MReg::V(v0) && rm == MReg::V(v0)
        )));

        // 3. Post-RA NEGATIVE controls: unknown upper bits (`lsr #31`, incoming arg `x0`, signed
        //    `ldrsw`, and `PINNED` `mov w4, w4`) MUST NOT be elided!
        let mut neg = new_post_ra();
        let nb0 = neg.new_block(false);
        neg.begin_block(nb0);
        // `lsr x2, x0, #31` can leave bit 32 set -> `mov w2, w2` must stay!
        neg.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X2), rn: MReg::p(PReg::X0), immr: 31, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X2), src: MReg::p(PReg::X2), wide: false },
            MInstFlags::NONE,
        );
        // Incoming arg `x0` has unknown upper 32 bits -> `mov w0, w0` must stay!
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X0), wide: false },
            MInstFlags::NONE,
        );
        // Signed `ldrsw x3, [x1, #0]` sign-extends bit 31 into bits 32..63 -> `mov w3, w3` must stay!
        neg.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X3),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 0 },
                size: MemSize::W32,
                signed: true,
            },
            MInstFlags::MEM_READ,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X3), src: MReg::p(PReg::X3), wide: false },
            MInstFlags::NONE,
        );
        // Pinned `mov w4, w4` after `lsr x4, x0, #32` -> must stay because PINNED!
        neg.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X4), rn: MReg::p(PReg::X0), immr: 32, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X4), src: MReg::p(PReg::X4), wide: false },
            MInstFlags::PINNED,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X4),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb0);

        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.movs_eliminated, 0, "none of the guarded narrow movs may be elided");
        let narrow_surviving =
            neg.ops.iter().filter(|op| matches!(**op, MOp::Mov { wide: false, .. })).count();
        assert_eq!(narrow_surviving, 4, "all 4 guarded `mov wR, wR` must survive");
        let neg_enc = mencode(&neg, alloc).expect("mencode negative zero-upper-32 must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function reading a valid `*const u32` and caller-saved regs.
            let f_pos: extern "C" fn(u64, *const u32) -> u64 = unsafe { pos_exec.as_fn() };
            let mem_u32: u32 = 10;
            assert_eq!(f_pos(0x0000_0005_0000_0123u64, &mem_u32), 191);

            let neg_bytes = words_to_bytes(&neg_enc.words, alloc);
            let neg_exec =
                blaze_inproc::exec::Executable::new(&neg_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function reading a valid `*const i32` and caller-saved regs.
            let f_neg: extern "C" fn(u64, *const i32) -> u64 = unsafe { neg_exec.as_fn() };
            let mem_i32: i32 = -1;
            assert_eq!(f_neg(0x8000_0000_0000_0005u64, &mem_i32), 0x0000_0001_0000_0004u64);
        }
        let _ = (pos_enc, neg_enc);
    }

    #[test]
    fn mopt_narrow_copy_prop_rewrites_32bit_consumers_and_respects_guards() {
        let alloc = test_alloc();

        // 1. Post-RA positive control (`fse_pack_sequences_leaf` pattern):
        //    `mov w9, w0; add w9, w1, w9; mov w10, w0; str w10, [x2, #0]; mov x0, x9; ret`
        //    -> `add w9, w1, w0; str w0, [x2, #0]; mov x0, x9; ret` (both narrow movs deleted!).
        let mut pos = new_post_ra();
        let pb0 = pos.new_block(false);
        pos.begin_block(pb0);
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X9), src: MReg::p(PReg::X0), wide: false },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X9),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X10), src: MReg::p(PReg::X0), wide: false },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X10),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X2), off: 0 },
                size: MemSize::W32,
            },
            MInstFlags::MEM_WRITE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X9), wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        pos.end_block(pb0);

        assert_eq!(mverify(&pos), Ok(()));
        let pos_stats = optimize_mfunc(&mut pos, alloc);
        assert_eq!(mverify(&pos), Ok(()));
        assert_eq!(pos_stats.movs_eliminated, 2, "both narrow copies must be eliminated");
        assert!(
            !pos.ops.iter().any(|op| matches!(*op, MOp::Mov { wide: false, .. })),
            "no narrow mov should remain"
        );
        assert!(pos.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg {
                dst: MReg::P(PReg::X9),
                rn: MReg::P(PReg::X1),
                rm: MReg::P(PReg::X0),
                shift: None,
                wide: false,
                sets_flags: false,
            }
        )));
        assert!(pos.ops.iter().any(|op| matches!(
            *op,
            MOp::Str {
                src: MReg::P(PReg::X0),
                addr: AddrMode::BaseOff { base: MReg::P(PReg::X2), off: 0 },
                size: MemSize::W32,
            }
        )));
        let pos_enc = mencode(&pos, alloc).expect("mencode narrow copy prop must succeed");

        // 2. Pre-RA SSA positive control with dirty upper-32 `src` (so zero-upper-32 does NOT
        //    promote it — strictly Peephole B fires!).
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v2 = ssa.alloc_vreg(VRegClass::Gpr32);
        let v3 = ssa.alloc_vreg(VRegClass::Gpr32);
        let sb0 = ssa.new_block(false);
        ssa.begin_block(sb0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 0x0000_0001_0000_0007u64, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v1), imm: 0x0000_0002_0000_0003u64, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v2), src: MReg::V(v0), wide: false },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v3),
                rn: MReg::V(v1),
                rm: MReg::V(v2),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v3), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(sb0);

        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.movs_eliminated >= 1, "SSA narrow copy must be eliminated");
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg { dst, rn, rm, wide: false, .. }
                if dst == MReg::V(v3) && rn == MReg::V(v1) && rm == MReg::V(v0)
        )));

        // 3. NEGATIVE guard 1: 64-bit consumer of `mov w9, w0` MUST NOT rewrite or delete!
        let mut neg_wide = new_post_ra();
        let wb0 = neg_wide.new_block(false);
        neg_wide.begin_block(wb0);
        neg_wide.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X9), src: MReg::p(PReg::X0), wide: false },
            MInstFlags::NONE,
        );
        neg_wide.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_wide.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_wide.end_block(wb0);

        assert_eq!(mverify(&neg_wide), Ok(()));
        let wide_stats = optimize_mfunc(&mut neg_wide, alloc);
        assert_eq!(mverify(&neg_wide), Ok(()));
        assert_eq!(wide_stats.movs_eliminated, 0, "64-bit consumer must preserve `mov w9, w0`");
        assert!(neg_wide.ops.iter().any(|op| matches!(
            *op,
            MOp::Mov { dst: MReg::P(PReg::X9), src: MReg::P(PReg::X0), wide: false }
        )));
        let neg_wide_enc = mencode(&neg_wide, alloc).expect("mencode neg_wide must succeed");

        // 4. NEGATIVE guard 2: `src` (`w0`) redefined before use of `w9` MUST NOT rewrite!
        let mut neg_redef = new_post_ra();
        let rb0 = neg_redef.new_block(false);
        neg_redef.begin_block(rb0);
        neg_redef.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X9), src: MReg::p(PReg::X0), wide: false },
            MInstFlags::NONE,
        );
        neg_redef.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_redef.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_redef.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_redef.end_block(rb0);

        assert_eq!(mverify(&neg_redef), Ok(()));
        let redef_stats = optimize_mfunc(&mut neg_redef, alloc);
        assert_eq!(mverify(&neg_redef), Ok(()));
        assert_eq!(redef_stats.movs_eliminated, 0, "redefined `src` must block narrow copy prop");
        assert!(neg_redef.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg {
                dst: MReg::P(PReg::X0),
                rn: MReg::P(PReg::X0),
                rm: MReg::P(PReg::X9),
                wide: false,
                ..
            }
        )));
        let neg_redef_enc = mencode(&neg_redef, alloc).expect("mencode neg_redef must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X2, X9, X10`.
            let f_pos: extern "C" fn(u64, u64, *mut u32) -> u64 = unsafe { pos_exec.as_fn() };
            let mut slot: u32 = 0;
            assert_eq!(
                f_pos(0xdead_beef_0000_0007u64, 0xcafe_babe_0000_0003u64, &mut slot),
                10
            );
            assert_eq!(slot, 7);

            let wide_bytes = words_to_bytes(&neg_wide_enc.words, alloc);
            let wide_exec =
                blaze_inproc::exec::Executable::new(&wide_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0, X1, X9`.
            let f_wide: extern "C" fn(u64, u64) -> u64 = unsafe { wide_exec.as_fn() };
            assert_eq!(f_wide(0x0000_0001_0000_0005u64, 10), 15);

            let redef_bytes = words_to_bytes(&neg_redef_enc.words, alloc);
            let redef_exec = blaze_inproc::exec::Executable::new(&redef_bytes, 0)
                .expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0, X9`.
            let f_redef: extern "C" fn(u64) -> u64 = unsafe { redef_exec.as_fn() };
            assert_eq!(f_redef(5), 11);
        }
        let _ = (pos_enc, neg_wide_enc, neg_redef_enc);
    }

    #[test]
    fn mopt_shift_into_alu_fusion_fuses_single_use_and_respects_guards() {
        let alloc = test_alloc();

        // 1. Pre-RA SSA positive control: `LSR` into `AddReg`, `LSL` into `AddReg` (commutative
        //    swap), `ASR` into `SubReg`, and `LSR` into `LogicalReg` (commutative swap).
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_r1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_r2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_r3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s4 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_r4 = ssa.alloc_vreg(VRegClass::Gpr64);
        let sb0 = ssa.new_block(false);
        ssa.begin_block(sb0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 0x1000, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v1), imm: 0x200, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        // `lsr v_s1, v0, #6; add v_r1, v1, v_s1`
        ssa.push_inst(
            MOp::Ubfm { dst: MReg::V(v_s1), rn: MReg::V(v0), immr: 6, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_r1),
                rn: MReg::V(v1),
                rm: MReg::V(v_s1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // `lsl v_s2, v0, #3` (`immr = 61, imms = 60`); `add v_r2, v_s2, v_r1` (swap!)
        ssa.push_inst(
            MOp::Ubfm { dst: MReg::V(v_s2), rn: MReg::V(v0), immr: 61, imms: 60, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_r2),
                rn: MReg::V(v_s2),
                rm: MReg::V(v_r1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // `asr v_s3, v1, #2; sub v_r3, v_r2, v_s3`
        ssa.push_inst(
            MOp::Sbfm { dst: MReg::V(v_s3), rn: MReg::V(v1), immr: 2, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::SubReg {
                dst: MReg::V(v_r3),
                rn: MReg::V(v_r2),
                rm: MReg::V(v_s3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // `lsr v_s4, v0, #1; eor v_r4, v_s4, v_r3` (swap!)
        ssa.push_inst(
            MOp::Ubfm { dst: MReg::V(v_s4), rn: MReg::V(v0), immr: 1, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::LogicalReg {
                dst: MReg::V(v_r4),
                rn: MReg::V(v_s4),
                rm: MReg::V(v_r3),
                shift: None,
                invert_rm: false,
                kind: LogicalKind::Eor,
                wide: true,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_r4), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(sb0);

        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.peepholes >= 4, "all 4 SSA single-use shifts must fuse");
        assert!(!ssa.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. } | MOp::Sbfm { .. })));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg { dst, rn, rm, shift: Some(MShift::Lsr(6)), wide: true, .. }
                if dst == MReg::V(v_r1) && rn == MReg::V(v1) && rm == MReg::V(v0)
        )));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg { dst, rn, rm, shift: Some(MShift::Lsl(3)), wide: true, .. }
                if dst == MReg::V(v_r2) && rn == MReg::V(v_r1) && rm == MReg::V(v0)
        )));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::SubReg { dst, rn, rm, shift: Some(MShift::Asr(2)), wide: true, .. }
                if dst == MReg::V(v_r3) && rn == MReg::V(v_r2) && rm == MReg::V(v1)
        )));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift: Some(MShift::Lsr(1)),
                invert_rm: false,
                kind: LogicalKind::Eor,
                wide: true,
            } if dst == MReg::V(v_r4) && rn == MReg::V(v_r3) && rm == MReg::V(v0)
        )));

        // 2. Post-RA positive control (`lz77_scan_default_leaf` pattern + `dst == tmp` swap):
        //    `lsr x9, x0, #6; add x2, x1, x9; lsl x10, x1, #2; add x10, x10, x2; mov x0, x10; ret`
        let mut pos = new_post_ra();
        let pb0 = pos.new_block(false);
        pos.begin_block(pb0);
        pos.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X0), immr: 6, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Ubfm {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X1),
                immr: 62,
                imms: 61,
                wide: true,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X10),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X10), wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        pos.end_block(pb0);

        assert_eq!(mverify(&pos), Ok(()));
        let pos_stats = optimize_mfunc(&mut pos, alloc);
        assert_eq!(mverify(&pos), Ok(()));
        assert!(pos_stats.peepholes >= 2, "both post-RA single-use shifts must fuse");
        assert!(!pos.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })));
        assert!(pos.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg {
                dst: MReg::P(PReg::X2),
                rn: MReg::P(PReg::X1),
                rm: MReg::P(PReg::X0),
                shift: Some(MShift::Lsr(6)),
                wide: true,
                sets_flags: false,
            }
        )));
        assert!(pos.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg {
                dst: MReg::P(PReg::X0),
                rn: MReg::P(PReg::X2),
                rm: MReg::P(PReg::X1),
                shift: Some(MShift::Lsl(2)),
                wide: true,
                sets_flags: false,
            }
        )));
        let pos_enc = mencode(&pos, alloc).expect("mencode post-RA shift fusion must succeed");

        // 3. NEGATIVE guard 1: multi-use shift MUST NOT fuse!
        let mut neg_multi = new_post_ra();
        let mb0 = neg_multi.new_block(false);
        neg_multi.begin_block(mb0);
        neg_multi.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X0), immr: 6, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        neg_multi.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_multi.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_multi.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_multi.end_block(mb0);

        assert_eq!(mverify(&neg_multi), Ok(()));
        let multi_stats = optimize_mfunc(&mut neg_multi, alloc);
        assert_eq!(mverify(&neg_multi), Ok(()));
        assert_eq!(multi_stats.peepholes, 0, "multi-use shift must NOT fuse");
        assert!(neg_multi.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })));
        let neg_multi_enc = mencode(&neg_multi, alloc).expect("mencode neg_multi must succeed");

        // 4. NEGATIVE guard 2: non-commutative `SubReg` (`tmp == rn`) and `LogicalReg { invert_rm: true }`
        //    (`tmp == rn`) MUST NOT swap or fuse!
        let mut neg_noncomm = new_post_ra();
        let ncb0 = neg_noncomm.new_block(false);
        neg_noncomm.begin_block(ncb0);
        neg_noncomm.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X0), immr: 6, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        neg_noncomm.push_inst(
            MOp::SubReg {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X9),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_noncomm.push_inst(
            MOp::Ubfm {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X0),
                immr: 3,
                imms: 63,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg_noncomm.push_inst(
            MOp::LogicalReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X10),
                rm: MReg::p(PReg::X2),
                shift: None,
                invert_rm: true,
                kind: LogicalKind::And,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg_noncomm.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_noncomm.end_block(ncb0);

        assert_eq!(mverify(&neg_noncomm), Ok(()));
        let noncomm_stats = optimize_mfunc(&mut neg_noncomm, alloc);
        assert_eq!(mverify(&neg_noncomm), Ok(()));
        assert_eq!(noncomm_stats.peepholes, 0, "non-commutative `rn` shift must NOT fuse");
        assert_eq!(
            neg_noncomm.ops.iter().filter(|op| matches!(**op, MOp::Ubfm { .. })).count(),
            2
        );
        let neg_noncomm_enc =
            mencode(&neg_noncomm, alloc).expect("mencode neg_noncomm must succeed");

        // 5. NEGATIVE guard 3: shift source `x0` redefined before consumer MUST NOT fuse!
        let mut neg_redef = new_post_ra();
        let rdb0 = neg_redef.new_block(false);
        neg_redef.begin_block(rdb0);
        neg_redef.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X0), immr: 6, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        neg_redef.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 64,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_redef.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_redef.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_redef.end_block(rdb0);

        assert_eq!(mverify(&neg_redef), Ok(()));
        let redef_stats = optimize_mfunc(&mut neg_redef, alloc);
        assert_eq!(mverify(&neg_redef), Ok(()));
        assert_eq!(redef_stats.peepholes, 0, "redefined shift source must block fusion");
        let neg_redef_enc = mencode(&neg_redef, alloc).expect("mencode neg_redef must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X2, X9, X10`.
            let f_pos: extern "C" fn(u64, u64) -> u64 = unsafe { pos_exec.as_fn() };
            assert_eq!(f_pos(640, 10), 60);

            let multi_bytes = words_to_bytes(&neg_multi_enc.words, alloc);
            let multi_exec = blaze_inproc::exec::Executable::new(&multi_bytes, 0)
                .expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X2, X9`.
            let f_multi: extern "C" fn(u64, u64) -> u64 = unsafe { multi_exec.as_fn() };
            assert_eq!(f_multi(640, 10), 30);

            let nc_bytes = words_to_bytes(&neg_noncomm_enc.words, alloc);
            let nc_exec =
                blaze_inproc::exec::Executable::new(&nc_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X2, X9, X10`.
            let f_nc: extern "C" fn(u64, u64) -> u64 = unsafe { nc_exec.as_fn() };
            assert_eq!(f_nc(640, 3), 80);

            let rd_bytes = words_to_bytes(&neg_redef_enc.words, alloc);
            let rd_exec =
                blaze_inproc::exec::Executable::new(&rd_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0, X9`.
            let f_rd: extern "C" fn(u64) -> u64 = unsafe { rd_exec.as_fn() };
            assert_eq!(f_rd(640), 714);
        }
        let _ = (pos_enc, neg_multi_enc, neg_noncomm_enc, neg_redef_enc);
    }

    #[test]
    fn mopt_branch_inversion_direct_and_across_empty_split_pre_and_post_ra() {
        let alloc = test_alloc();

        // 1. Pre-RA SSA positive control: `cmp v2, v0; b.hs b_exit; b b_loop` where `b_exit` is
        //    the next block in `block_order` -> inverts to `b.lo b_loop` preserving `vflag_use`
        //    and `TERMINATOR`, deleting trailing `B` and rebuilding CFG.
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let b_entry = ssa.new_block(false);
        let b_loop = ssa.new_block(true);
        let b_exit = ssa.new_block(false);

        ssa.begin_block(b_entry);
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v0), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v2), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        ssa.end_block(b_entry);

        ssa.begin_block(b_loop);
        let s_cmp = ssa.push_inst(
            MOp::CmpReg { rn: MReg::V(v2), rm: MReg::V(v0), shift: None, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(s_cmp, 0);
        let s_bhs = ssa.push_inst(
            MOp::BCond { cond: MCond::Hs, target: b_exit },
            MInstFlags::TERMINATOR,
        );
        ssa.set_vflag_use(s_bhs, 0);
        ssa.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        ssa.end_block(b_loop);

        ssa.begin_block(b_exit);
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v2), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(b_exit);

        ssa.rebuild_cfg(alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.branches_folded >= 2, "expected fallthrough B elision + BCond inversion");
        assert!(!ssa.ops.iter().any(|op| matches!(*op, MOp::B { .. })), "all trailing Bs must fold");
        let s_loop_range = ssa.block_inst_range(b_loop);
        assert_eq!(s_loop_range.len(), 2);
        let inv_idx = s_loop_range.start + 1;
        assert_eq!(ssa.ops[inv_idx], MOp::BCond { cond: MCond::Lo, target: b_loop });
        assert_eq!(ssa.vflag_use[inv_idx], 0, "vflag_use must remain intact after inversion");
        assert!(ssa.flags[inv_idx].contains(MInstFlags::TERMINATOR));
        assert_eq!(ssa.block_succs(b_loop), &[b_loop, b_exit]);

        // 2. Post-RA test: `Tbz` inversion + `BCond` inversion ACROSS an empty edge-split block
        //    `b_split` (`b b_exit`) sitting between `b_loop` and `b_exit` (`decode_exec_leaf_single`
        //    inner memcpy loop pattern).
        //    Signature: `fn(n: u64, skip_flag: u64) -> u64` (`X0`, `X1`).
        let mut post = new_post_ra();
        let p_entry = post.new_block(false);
        let p_loop = post.new_block(true);
        let p_split = post.new_block(false);
        let p_exit = post.new_block(false);

        post.begin_block(p_entry);
        post.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X2), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        post.push_inst(
            MOp::Tbz { rt: MReg::p(PReg::X1), bit: 0, target: p_loop },
            MInstFlags::TERMINATOR,
        );
        post.push_inst(MOp::B { target: p_exit }, MInstFlags::TERMINATOR);
        post.end_block(p_entry);

        post.begin_block(p_loop);
        post.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let p_cmp = post.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X0),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        post.set_vflag_def(p_cmp, 0);
        let p_bhs = post.push_inst(
            MOp::BCond { cond: MCond::Hs, target: p_split },
            MInstFlags::TERMINATOR,
        );
        post.set_vflag_use(p_bhs, 0);
        post.push_inst(MOp::B { target: p_loop }, MInstFlags::TERMINATOR);
        post.end_block(p_loop);

        post.begin_block(p_split);
        post.push_inst(MOp::B { target: p_exit }, MInstFlags::TERMINATOR);
        post.end_block(p_split);

        post.begin_block(p_exit);
        post.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X2), wide: true },
            MInstFlags::NONE,
        );
        post.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        post.end_block(p_exit);

        post.rebuild_cfg(alloc);
        assert_eq!(mverify(&post), Ok(()));
        let post_stats = optimize_mfunc(&mut post, alloc);
        assert_eq!(mverify(&post), Ok(()));
        assert!(post_stats.branches_folded >= 3);
        assert!(!post.ops.iter().any(|op| matches!(*op, MOp::B { .. })));

        let p_entry_r = post.block_inst_range(p_entry);
        assert_eq!(
            post.ops[p_entry_r.end - 1],
            MOp::Tbnz { rt: MReg::p(PReg::X1), bit: 0, target: p_exit }
        );
        let p_loop_r = post.block_inst_range(p_loop);
        let p_inv = p_loop_r.end - 1;
        assert_eq!(post.ops[p_inv], MOp::BCond { cond: MCond::Lo, target: p_loop });
        assert_eq!(post.vflag_use[p_inv], 0);
        assert!(post.flags[p_inv].contains(MInstFlags::TERMINATOR));

        let post_enc = mencode(&post, alloc).expect("mencode inverted branch loop must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&post_enc.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X2`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(5, 0), 5);
            assert_eq!(f(1, 0), 1);
            assert_eq!(f(5, 1), 0);
        }
        let _ = post_enc;
    }

    #[test]
    fn mopt_global_multi_inst_mov_imm_cse_hoists_and_respects_guards() {
        let alloc = test_alloc();
        const HASH_K: u64 = 0x9e37_79b1_85eb_ca87;
        const LOOP_K: u64 = 0x0001_0002_0003_0000;
        const SMALL_K: u64 = 0xff;

        assert_eq!(mov_imm_inst_cost(HASH_K, true), 4);
        assert_eq!(mov_imm_inst_cost(LOOP_K, true), 3);
        assert_eq!(mov_imm_inst_cost(SMALL_K, true), 1);

        // 1. Positive SSA control: `HASH_K` appears in 3 blocks (including inside a `ParallelCopy`
        //    src!) and `LOOP_K` (`cost == 3`) appears once in a loop block; `SMALL_K` (`cost == 1`)
        //    appears in 2 blocks and MUST NOT be hoisted.
        let mut ssa = new_ssa();
        let v_arg = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_cnt = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_t0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_h1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_lk = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_p1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_p2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_p3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_h2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_h3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_phi = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_out = ssa.alloc_vreg(VRegClass::Gpr64);

        let b_entry = ssa.new_block(false);
        let b_loop = ssa.new_block(true);
        let b_mid = ssa.new_block(false);
        let b_exit = ssa.new_block(false);

        ssa.begin_block(b_entry);
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v_arg), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_cnt), imm: 4, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_s0), imm: SMALL_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_t0),
                rn: MReg::V(v_arg),
                rm: MReg::V(v_s0),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp = ssa.push_inst(
            MOp::CmpImm { rn: MReg::V(v_t0), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(cmp, 0);
        let beq = ssa.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_mid },
            MInstFlags::TERMINATOR,
        );
        ssa.set_vflag_use(beq, 0);
        ssa.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        ssa.end_block(b_entry);

        ssa.begin_block(b_loop);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_h1), imm: HASH_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_lk), imm: LOOP_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_s1), imm: SMALL_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::Mul { dst: MReg::V(v_p1), rn: MReg::V(v_t0), rm: MReg::V(v_h1), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_p2),
                rn: MReg::V(v_p1),
                rm: MReg::V(v_lk),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_p3),
                rn: MReg::V(v_p2),
                rm: MReg::V(v_s1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_parallel_copy(&[(MReg::V(v_phi), MReg::V(v_h1))], MInstFlags::NONE);
        ssa.push_inst(
            MOp::Cbnz { rt: MReg::V(v_cnt), target: b_loop, wide: true },
            MInstFlags::TERMINATOR,
        );
        ssa.push_inst(MOp::B { target: b_exit }, MInstFlags::TERMINATOR);
        ssa.end_block(b_loop);

        ssa.begin_block(b_mid);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_h2), imm: HASH_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_parallel_copy(&[(MReg::V(v_phi), MReg::V(v_h2))], MInstFlags::NONE);
        ssa.push_inst(MOp::B { target: b_exit }, MInstFlags::TERMINATOR);
        ssa.end_block(b_mid);

        ssa.begin_block(b_exit);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_h3), imm: HASH_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_out),
                rn: MReg::V(v_phi),
                rm: MReg::V(v_h3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::V(v_out),
                rm: MReg::V(v_p3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(b_exit);

        ssa.rebuild_cfg(alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        let stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(stats.peepholes >= 4, "3 HASH_K duplicates + 1 in-loop LOOP_K must hoist");

        // Exactly ONE `HASH_K` and ONE `LOOP_K` survive globally, and both reside in `b_entry`.
        let total_hash = ssa
            .ops
            .iter()
            .filter(|op| matches!(**op, MOp::MovImm { imm: HASH_K, wide: true, .. }))
            .count();
        let total_loop_k = ssa
            .ops
            .iter()
            .filter(|op| matches!(**op, MOp::MovImm { imm: LOOP_K, wide: true, .. }))
            .count();
        assert_eq!(total_hash, 1, "HASH_K must be hoisted to a single entry MovImm");
        assert_eq!(total_loop_k, 1, "in-loop 3-inst LOOP_K must be hoisted to a single entry MovImm");

        let entry_range = ssa.block_inst_range(b_entry);
        let entry_start = entry_range.start;
        let entry_end = entry_range.end;
        let hoisted_hash_reg = ssa.ops[entry_start..entry_end]
            .iter()
            .find_map(|op| match *op {
                MOp::MovImm { dst, imm: HASH_K, wide: true } => Some(dst),
                _ => None,
            })
            .expect("HASH_K must be defined in entry_blk");
        assert!(
            ssa.ops[entry_start..entry_end].iter().any(|op| matches!(
                *op,
                MOp::MovImm { imm: LOOP_K, wide: true, .. }
            )),
            "LOOP_K must be defined in entry_blk"
        );
        // `SMALL_K` (`cost == 1`) stays in `b_loop` (not globally hoisted).
        let loop_range = ssa.block_inst_range(b_loop);
        assert!(
            ssa.ops[loop_range].iter().any(|op| matches!(
                *op,
                MOp::MovImm { imm: SMALL_K, wide: true, .. }
            )),
            "1-inst SMALL_K must remain unhoisted in b_loop"
        );
        // Both `ParallelCopy` sources for `v_phi` must have been remapped to `hoisted_hash_reg`.
        let mut remapped_copies: usize = 0;
        for &(dst, src) in &ssa.copy_pairs {
            if dst == MReg::V(v_phi) {
                assert_eq!(src, hoisted_hash_reg);
                remapped_copies += 1;
            }
        }
        assert_eq!(remapped_copies, 2);

        // 2. Negative guard: `entry_blk` has a back-edge into itself -> global hoist MUST skip!
        let mut neg = new_ssa();
        let n_arg = neg.alloc_vreg(VRegClass::Gpr64);
        let n_h0 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_sum0 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_h1 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_sum1 = neg.alloc_vreg(VRegClass::Gpr64);
        let nb0 = neg.new_block(false);
        let nb1 = neg.new_block(false);

        neg.begin_block(nb0);
        neg.push_inst(
            MOp::Mov { dst: MReg::V(n_arg), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::V(n_h0), imm: HASH_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_sum0),
                rn: MReg::V(n_arg),
                rm: MReg::V(n_h0),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Cbnz { rt: MReg::V(n_sum0), target: nb0, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.end_block(nb0);

        neg.begin_block(nb1);
        neg.push_inst(
            MOp::MovImm { dst: MReg::V(n_h1), imm: HASH_K, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_sum1),
                rn: MReg::V(n_sum0),
                rm: MReg::V(n_h1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(n_sum1), wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb1);

        neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let _ = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let neg_hash_count = neg
            .ops
            .iter()
            .filter(|op| matches!(**op, MOp::MovImm { imm: HASH_K, wide: true, .. }))
            .count();
        assert_eq!(neg_hash_count, 2, "entry with back-edge must skip global MovImm hoisting");
    }

    #[test]
    fn mopt_dead_narrow32_mov_elision_pre_ra_and_post_ra() {
        let alloc = test_alloc();

        // 1. Pre-RA SSA positive control: `v_lsr = lsr x_v0, x_v1; v_trunc = mov w_v_lsr` used
        //    strictly in 32-bit ops (`AddReg { wide: false }` + `Str(W32)`) in a DIFFERENT block!
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_ptr = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_lsr = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_trunc = ssa.alloc_vreg(VRegClass::Gpr32);
        let v_sum = ssa.alloc_vreg(VRegClass::Gpr32);
        let sb0 = ssa.new_block(false);
        let sb1 = ssa.new_block(false);

        ssa.begin_block(sb0);
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v0), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v1), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v2), src: MReg::p(PReg::X2), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v_ptr), src: MReg::p(PReg::X3), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::ShiftVar {
                dst: MReg::V(v_lsr),
                rn: MReg::V(v0),
                rm: MReg::V(v1),
                kind: ShiftKind::Lsr,
                wide: true,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::V(v_trunc), src: MReg::V(v_lsr), wide: false },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::B { target: sb1 }, MInstFlags::TERMINATOR);
        ssa.end_block(sb0);

        ssa.begin_block(sb1);
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_sum),
                rn: MReg::V(v2),
                rm: MReg::V(v_trunc),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Str {
                src: MReg::V(v_trunc),
                addr: AddrMode::BaseOff { base: MReg::V(v_ptr), off: 0 },
                size: MemSize::W32,
            },
            MInstFlags::MEM_WRITE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_sum), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(sb1);

        ssa.rebuild_cfg(alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.movs_eliminated >= 1, "cross-block SSA narrow mov must be elided");
        assert!(
            !ssa.ops.iter().any(|op| matches!(*op, MOp::Mov { dst, wide: false, .. } if dst == MReg::V(v_trunc))),
            "v_trunc narrow mov must be deleted"
        );
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::AddReg { dst, rn, rm, wide: false, .. }
                if dst == MReg::V(v_sum) && rn == MReg::V(v2) && rm == MReg::V(v_lsr)
        )));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::Str { src, size: MemSize::W32, .. } if src == MReg::V(v_lsr)
        )));

        // 2. Pre-RA SSA negative guard: narrow mov used in `ParallelCopy` or 64-bit `AddReg`
        //    MUST NOT be elided!
        let mut ssa_neg = new_ssa();
        let nv0 = ssa_neg.alloc_vreg(VRegClass::Gpr64);
        let nv1 = ssa_neg.alloc_vreg(VRegClass::Gpr64);
        let nv_lsr = ssa_neg.alloc_vreg(VRegClass::Gpr64);
        let nv_t_pc = ssa_neg.alloc_vreg(VRegClass::Gpr32);
        let nv_t_64 = ssa_neg.alloc_vreg(VRegClass::Gpr32);
        let nv_phi = ssa_neg.alloc_vreg(VRegClass::Gpr64);
        let nv_out = ssa_neg.alloc_vreg(VRegClass::Gpr64);
        let nsb0 = ssa_neg.new_block(false);
        let nsb1 = ssa_neg.new_block(false);

        ssa_neg.begin_block(nsb0);
        ssa_neg.push_inst(
            MOp::MovImm { dst: MReg::V(nv0), imm: 0x0001_0000_0000_0050u64, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa_neg.push_inst(
            MOp::MovImm { dst: MReg::V(nv1), imm: 4, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa_neg.push_inst(
            MOp::ShiftVar {
                dst: MReg::V(nv_lsr),
                rn: MReg::V(nv0),
                rm: MReg::V(nv1),
                kind: ShiftKind::Lsr,
                wide: true,
            },
            MInstFlags::NONE,
        );
        ssa_neg.push_inst(
            MOp::Mov { dst: MReg::V(nv_t_pc), src: MReg::V(nv_lsr), wide: false },
            MInstFlags::NONE,
        );
        ssa_neg.push_inst(
            MOp::Mov { dst: MReg::V(nv_t_64), src: MReg::V(nv_lsr), wide: false },
            MInstFlags::NONE,
        );
        ssa_neg.push_parallel_copy(&[(MReg::V(nv_phi), MReg::V(nv_t_pc))], MInstFlags::NONE);
        ssa_neg.push_inst(MOp::B { target: nsb1 }, MInstFlags::TERMINATOR);
        ssa_neg.end_block(nsb0);

        ssa_neg.begin_block(nsb1);
        ssa_neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(nv_out),
                rn: MReg::V(nv_phi),
                rm: MReg::V(nv_t_64),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa_neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(nv_out), wide: true },
            MInstFlags::NONE,
        );
        ssa_neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa_neg.end_block(nsb1);

        ssa_neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&ssa_neg), Ok(()));
        let ssa_neg_stats = optimize_mfunc(&mut ssa_neg, alloc);
        assert_eq!(mverify(&ssa_neg), Ok(()));
        assert_eq!(ssa_neg_stats.movs_eliminated, 0);
        assert_eq!(
            ssa_neg.ops.iter().filter(|op| matches!(**op, MOp::Mov { wide: false, .. })).count(),
            2
        );

        // 3. Post-RA positive control (`decode_exec_leaf_single` pattern):
        //    `lsr x5, x0, x1; mov w5, w5; add w9, w2, w5; str w5, [x3, #0]; mov x5, #0; mov x0, x9; ret`
        let mut pos = new_post_ra();
        let pb0 = pos.new_block(false);
        pos.begin_block(pb0);
        pos.push_inst(
            MOp::ShiftVar {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                kind: ShiftKind::Lsr,
                wide: true,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X5), src: MReg::p(PReg::X5), wide: false },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X9),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X5),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X5),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X3), off: 0 },
                size: MemSize::W32,
            },
            MInstFlags::MEM_WRITE,
        );
        pos.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X5), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X9), wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        pos.end_block(pb0);

        assert_eq!(mverify(&pos), Ok(()));
        let pos_stats = optimize_mfunc(&mut pos, alloc);
        assert_eq!(mverify(&pos), Ok(()));
        assert!(pos_stats.movs_eliminated >= 1, "post-RA dead `mov w5, w5` must be elided");
        assert!(!pos.ops.iter().any(|op| matches!(*op, MOp::Mov { wide: false, .. })));
        let pos_enc = mencode(&pos, alloc).expect("mencode post-RA dead mov w5 must succeed");

        // 4. Post-RA negative guards: (a) 64-bit read before overwrite, (b) escapes block without
        //    overwrite -> BOTH must preserve `mov w5, w5`!
        let mut neg64 = new_post_ra();
        let n64_b0 = neg64.new_block(false);
        neg64.begin_block(n64_b0);
        neg64.push_inst(
            MOp::ShiftVar {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                kind: ShiftKind::Lsr,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg64.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X5), src: MReg::p(PReg::X5), wide: false },
            MInstFlags::NONE,
        );
        neg64.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X5),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg64.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X5), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        neg64.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg64.end_block(n64_b0);

        assert_eq!(mverify(&neg64), Ok(()));
        let n64_stats = optimize_mfunc(&mut neg64, alloc);
        assert_eq!(mverify(&neg64), Ok(()));
        assert_eq!(n64_stats.movs_eliminated, 0);
        assert!(neg64.ops.iter().any(|op| matches!(
            *op,
            MOp::Mov { dst: MReg::P(PReg::X5), src: MReg::P(PReg::X5), wide: false }
        )));
        let neg64_enc = mencode(&neg64, alloc).expect("mencode neg64 must succeed");

        let mut neg_esc = new_post_ra();
        let ne_b0 = neg_esc.new_block(false);
        let ne_b1 = neg_esc.new_block(false);
        neg_esc.begin_block(ne_b0);
        neg_esc.push_inst(
            MOp::ShiftVar {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                kind: ShiftKind::Lsr,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg_esc.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X5), src: MReg::p(PReg::X5), wide: false },
            MInstFlags::NONE,
        );
        neg_esc.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X9),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X5),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_esc.push_inst(MOp::B { target: ne_b1 }, MInstFlags::TERMINATOR);
        neg_esc.end_block(ne_b0);

        neg_esc.begin_block(ne_b1);
        neg_esc.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X5),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_esc.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_esc.end_block(ne_b1);

        neg_esc.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg_esc), Ok(()));
        let esc_stats = optimize_mfunc(&mut neg_esc, alloc);
        assert_eq!(mverify(&neg_esc), Ok(()));
        assert_eq!(esc_stats.movs_eliminated, 0, "x5 escapes block without overwrite");
        assert!(neg_esc.ops.iter().any(|op| matches!(
            *op,
            MOp::Mov { dst: MReg::P(PReg::X5), src: MReg::P(PReg::X5), wide: false }
        )));

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X3, X5, X9`.
            let f_pos: extern "C" fn(u64, u64, u64, *mut u32) -> u64 = unsafe { pos_exec.as_fn() };
            let mut out: u32 = 0;
            assert_eq!(f_pos(0xdead_beef_0000_0050u64, 4, 10, &mut out), 0xf000_000fu64);
            assert_eq!(out, 0xf000_0005u32);

            let n64_bytes = words_to_bytes(&neg64_enc.words, alloc);
            let n64_exec =
                blaze_inproc::exec::Executable::new(&n64_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X2, X5`.
            let f_n64: extern "C" fn(u64, u64, u64) -> u64 = unsafe { n64_exec.as_fn() };
            assert_eq!(f_n64(0x0000_0001_0000_0050u64, 0, 1), 0x51);
        }
        let _ = (pos_enc, neg64_enc);
    }

    #[test]
    fn mopt_post_ra_sp_spill_redundant_load_elimination() {
        let alloc = test_alloc();
        // Signature: `fn(a: u64, b: u64, c: u64, d: u64, ext_ptr: *mut u64) -> u64` (`X0..X4`).
        let mut func = new_post_ra();
        let b0 = func.new_block(false);
        func.begin_block(b0);

        // Allocate 64-byte aligned stack frame (`PINNED`).
        func.push_inst(
            MOp::SubImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: 64,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::PINNED,
        );
        // 1. `str x0, [sp, #0]; ldr x0, [sp, #0]` -> `d == s` deletes redundant `ldr` completely!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X0),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 0 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X0),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        // 2. Non-overlapping `[sp, #16]` store preserves `[sp, #8]`; `ldr x16, [sp, #8]` forwards!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X1),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 8 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X2),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 16 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X16),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        // 3. `mov x9, x16` aliases `[sp, #8]` into `x9`; clobbering `x16` still lets `ldr x17, [sp, #8]` forward!
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X9), src: MReg::p(PReg::X16), wide: true },
            MInstFlags::NONE,
        );
        // 4. `str w3, [sp, #24]; ldr w3, [sp, #24]` forwards + dead-narrow32 cleanup removes `mov w3, w3`!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X3),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 24 },
                size: MemSize::W32,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X3),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 24 },
                size: MemSize::W32,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X3),
                imm12: 1,
                shift12: false,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X3), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X16), imm: 99, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X1), imm: 77, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X17),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        // 5. Non-`SP` store `str x0, [x4, #0]` MUST invalidate all `[sp, #off]` entries (soundness
        //    against taken-address stack slots!), so `ldr x11, [sp, #0]` MUST survive!
        func.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X0),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X4), off: 0 },
                size: MemSize::X64,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X11),
                addr: AddrMode::BaseOff { base: MReg::SP, off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        // Sum: `x0 = x17 + x11 + x10 + x9`, restore `sp`, `ret`.
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X17),
                rm: MReg::p(PReg::X11),
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
                rm: MReg::p(PReg::X10),
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
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: 64,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::PINNED,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats.loads_forwarded, 4, "all 4 redundant [sp, #off] loads must forward/elide");

        let surviving_ldrs = func.ops.iter().filter(|op| matches!(**op, MOp::Ldr { .. })).count();
        assert_eq!(surviving_ldrs, 1, "only post-non-SP-store [sp, #0] reload may survive");
        assert!(
            !func.ops.iter().any(|op| matches!(*op, MOp::Mov { wide: false, .. })),
            "forwarded w3 narrow self-mov must be elided by dead-narrow32 cleanup"
        );

        let enc_out = mencode(&func, alloc).expect("mencode [sp, #off] spill elim must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function with balanced 64-byte SP frame touching only
            // caller-saved `X0..X4, X9..X11, X16, X17`.
            let f: extern "C" fn(u64, u64, u64, u64, *mut u64) -> u64 = unsafe { exec.as_fn() };
            let mut ext: u64 = 0;
            assert_eq!(f(10, 20, 30, 0xdead_beef_0000_0005u64, &mut ext), 56);
            assert_eq!(ext, 10);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_inverts_backward_loop_backedge_over_forward_exit() {
        let alloc = test_alloc();

        // 1. Post-RA exact `[CmpReg, BCond(Hi -> cold_exit), B(loop_hdr)]` with `loop_hdr` at
        //    `ord_pos 0`, a non-pass-through intervening block at `ord_pos 1`, and `cold_exit` at
        //    `ord_pos 2` (so `cold_exit` is NOT `next_blk`).
        //    Must invert to `[CmpReg, BCond(Ls -> loop_hdr), B(cold_exit)]`, preserve `vflag_use`
        //    and `TERMINATOR`, pass `mverify` + `mencode`, and be idempotent on a second pass.
        let mut func = new_post_ra();
        let loop_hdr = func.new_block(true);
        let b_mid = func.new_block(false);
        let cold_exit = func.new_block(false);

        func.begin_block(loop_hdr);
        let cmp = func.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let bhi = func.push_inst(
            MOp::BCond { cond: MCond::Hi, target: cold_exit },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(bhi, 0);
        func.push_inst(MOp::B { target: loop_hdr }, MInstFlags::TERMINATOR);
        func.end_block(loop_hdr);

        func.begin_block(b_mid);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 999, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_mid);

        func.begin_block(cold_exit);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(cold_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats.branches_folded, 1, "hot backedge branch pair must invert once");

        let hdr_r = func.block_inst_range(loop_hdr);
        assert_eq!(hdr_r.len(), 3);
        assert_eq!(
            func.ops[hdr_r.start],
            MOp::CmpReg {
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
            }
        );
        assert_eq!(func.ops[hdr_r.start + 1], MOp::BCond { cond: MCond::Ls, target: loop_hdr });
        assert_eq!(func.vflag_use[hdr_r.start + 1], 0, "vflag_use must remain intact");
        assert!(func.flags[hdr_r.start + 1].contains(MInstFlags::TERMINATOR));
        assert_eq!(func.ops[hdr_r.start + 2], MOp::B { target: cold_exit });
        assert!(func.flags[hdr_r.start + 2].contains(MInstFlags::TERMINATOR));
        assert_eq!(func.block_succs(loop_hdr), &[loop_hdr, cold_exit]);

        let _enc = mencode(&func, alloc).expect("mencode inverted backedge must succeed");

        // Second pass must be strictly idempotent (never flip back!).
        let stats2 = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats2.branches_folded, 0, "already-inverted backedge must not flip again");
        assert_eq!(func.ops[hdr_r.start + 1], MOp::BCond { cond: MCond::Ls, target: loop_hdr });
        assert_eq!(func.ops[hdr_r.start + 2], MOp::B { target: cold_exit });

        // 2. Pre-RA SSA positive control with the exact same 3-block layout.
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let s_hdr = ssa.new_block(true);
        let s_mid = ssa.new_block(false);
        let s_exit = ssa.new_block(false);

        ssa.begin_block(s_hdr);
        let s_cmp = ssa.push_inst(
            MOp::CmpReg { rn: MReg::V(v0), rm: MReg::V(v1), shift: None, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(s_cmp, 0);
        let s_bhi = ssa.push_inst(
            MOp::BCond { cond: MCond::Hi, target: s_exit },
            MInstFlags::TERMINATOR,
        );
        ssa.set_vflag_use(s_bhi, 0);
        ssa.push_inst(MOp::B { target: s_hdr }, MInstFlags::TERMINATOR);
        ssa.end_block(s_hdr);

        ssa.begin_block(s_mid);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 7, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(s_mid);

        ssa.begin_block(s_exit);
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v0), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(s_exit);

        ssa.rebuild_cfg(alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert_eq!(ssa_stats.branches_folded, 1, "SSA hot backedge branch pair must invert");

        let s_hdr_r = ssa.block_inst_range(s_hdr);
        assert_eq!(s_hdr_r.len(), 3);
        assert_eq!(ssa.ops[s_hdr_r.start + 1], MOp::BCond { cond: MCond::Ls, target: s_hdr });
        assert_eq!(ssa.vflag_use[s_hdr_r.start + 1], 0);
        assert!(ssa.flags[s_hdr_r.start + 1].contains(MInstFlags::TERMINATOR));
        assert_eq!(ssa.ops[s_hdr_r.start + 2], MOp::B { target: s_exit });
        assert_eq!(ssa.block_succs(s_hdr), &[s_hdr, s_exit]);

        // 3. Post-RA executable loop control verifying Apple Silicon hardware semantics:
        //    `loop { x0 += 1; if x0 > x1 { return x0; } }` with intervening `ord_pos 1` block.
        let mut exec_fn = new_post_ra();
        let e_hdr = exec_fn.new_block(true);
        let e_mid = exec_fn.new_block(false);
        let e_exit = exec_fn.new_block(false);

        exec_fn.begin_block(e_hdr);
        exec_fn.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let e_cmp = exec_fn.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        exec_fn.set_vflag_def(e_cmp, 0);
        let e_bhi = exec_fn.push_inst(
            MOp::BCond { cond: MCond::Hi, target: e_exit },
            MInstFlags::TERMINATOR,
        );
        exec_fn.set_vflag_use(e_bhi, 0);
        exec_fn.push_inst(MOp::B { target: e_hdr }, MInstFlags::TERMINATOR);
        exec_fn.end_block(e_hdr);

        exec_fn.begin_block(e_mid);
        exec_fn.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 999, wide: true },
            MInstFlags::NONE,
        );
        exec_fn.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        exec_fn.end_block(e_mid);

        exec_fn.begin_block(e_exit);
        exec_fn.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        exec_fn.end_block(e_exit);

        exec_fn.rebuild_cfg(alloc);
        assert_eq!(mverify(&exec_fn), Ok(()));
        let e_stats = optimize_mfunc(&mut exec_fn, alloc);
        assert_eq!(mverify(&exec_fn), Ok(()));
        assert_eq!(e_stats.branches_folded, 1);

        let e_hdr_r = exec_fn.block_inst_range(e_hdr);
        assert_eq!(e_hdr_r.len(), 4);
        assert_eq!(exec_fn.ops[e_hdr_r.start + 2], MOp::BCond { cond: MCond::Ls, target: e_hdr });
        assert_eq!(exec_fn.vflag_use[e_hdr_r.start + 2], 0);
        assert_eq!(exec_fn.ops[e_hdr_r.start + 3], MOp::B { target: e_exit });

        let e_enc = mencode(&exec_fn, alloc).expect("mencode executable inverted loop must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&e_enc.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`, `X1`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(0, 5), 6);
            assert_eq!(f(5, 5), 6);
            assert_eq!(f(10, 5), 11);
        }
        let _ = e_enc;

        // 4. Inner self-loop at `ord_pos 1` whose conditional exit jumps back to an outer latch at
        //    `ord_pos 0` (`cond_pos < ord_pos`) while trailing `B` targets itself (`uncond_pos == ord_pos`):
        //    `[BCond(Hs -> ord_pos 0), B(ord_pos 1)]` must invert to
        //    `[BCond(Lo -> ord_pos 1), B(ord_pos 0)]`, pass `mverify` + `mencode`, and be idempotent.
        let mut self_fn = new_post_ra();
        let outer_latch = self_fn.new_block(false);
        let inner_self = self_fn.new_block(true);
        let self_exit = self_fn.new_block(false);

        self_fn.begin_block(outer_latch);
        self_fn.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X3),
                rn: MReg::p(PReg::X3),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        self_fn.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X1), target: self_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        self_fn.end_block(outer_latch);

        self_fn.begin_block(inner_self);
        self_fn.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let scmp = self_fn.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        self_fn.set_vflag_def(scmp, 0);
        let sbhs = self_fn.push_inst(
            MOp::BCond { cond: MCond::Hs, target: outer_latch },
            MInstFlags::TERMINATOR,
        );
        self_fn.set_vflag_use(sbhs, 0);
        self_fn.push_inst(MOp::B { target: inner_self }, MInstFlags::TERMINATOR);
        self_fn.end_block(inner_self);

        self_fn.begin_block(self_exit);
        self_fn.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        self_fn.end_block(self_exit);

        self_fn.rebuild_cfg(alloc);
        assert_eq!(mverify(&self_fn), Ok(()));
        let self_stats = optimize_mfunc(&mut self_fn, alloc);
        assert_eq!(mverify(&self_fn), Ok(()));
        assert_eq!(self_stats.branches_folded, 1, "inner self-loop back-edge must invert once");

        let sr = self_fn.block_inst_range(inner_self);
        assert_eq!(sr.len(), 4);
        assert_eq!(self_fn.ops[sr.start + 2], MOp::BCond { cond: MCond::Lo, target: inner_self });
        assert_eq!(self_fn.vflag_use[sr.start + 2], 0, "vflag_use must remain intact");
        assert!(self_fn.flags[sr.start + 2].contains(MInstFlags::TERMINATOR));
        assert_eq!(self_fn.ops[sr.start + 3], MOp::B { target: outer_latch });
        assert!(self_fn.flags[sr.start + 3].contains(MInstFlags::TERMINATOR));
        assert_eq!(self_fn.block_succs(inner_self), &[inner_self, outer_latch]);

        let _self_enc = mencode(&self_fn, alloc).expect("mencode inverted self-loop must succeed");

        let self_stats2 = optimize_mfunc(&mut self_fn, alloc);
        assert_eq!(mverify(&self_fn), Ok(()));
        assert_eq!(self_stats2.branches_folded, 0, "already-inverted self-loop must not flip back");
        assert_eq!(self_fn.ops[sr.start + 2], MOp::BCond { cond: MCond::Lo, target: inner_self });
        assert_eq!(self_fn.ops[sr.start + 3], MOp::B { target: outer_latch });
    }

    #[test]
    fn mopt_fuses_neg_add_into_sub_ssa() {
        let alloc = test_alloc();

        // 1. SSA positive controls:
        //    (a) `neg v_t1, v1, lsr #3; add v_d1, v0, v_t1` -> `sub v_d1, v0, v1, lsr #3`
        //    (b) `neg v_t2, v1; add v_d2, v_t2, v_d1` (commutative swap) -> `sub v_d2, v_d1, v1`
        //    (c) `lsr v_s, v1, #2; neg v_t3, v_s; add v_d3, v_d2, v_t3` (chains shift + neg + add!)
        //        -> `sub v_d3, v_d2, v1, lsr #2`
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_t1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_d1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_t2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_d2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_t3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_d3 = ssa.alloc_vreg(VRegClass::Gpr64);
        let b0 = ssa.new_block(false);

        ssa.begin_block(b0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 0x1000, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v1), imm: 0x200, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::SubReg {
                dst: MReg::V(v_t1),
                rn: MReg::XZR,
                rm: MReg::V(v1),
                shift: Some(MShift::Lsr(3)),
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_d1),
                rn: MReg::V(v0),
                rm: MReg::V(v_t1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::SubReg {
                dst: MReg::V(v_t2),
                rn: MReg::XZR,
                rm: MReg::V(v1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_d2),
                rn: MReg::V(v_t2),
                rm: MReg::V(v_d1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Ubfm { dst: MReg::V(v_s), rn: MReg::V(v1), immr: 2, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::SubReg {
                dst: MReg::V(v_t3),
                rn: MReg::XZR,
                rm: MReg::V(v_s),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_d3),
                rn: MReg::V(v_d2),
                rm: MReg::V(v_t3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_d3), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(b0);

        assert_eq!(mverify(&ssa), Ok(()));
        let stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(stats.peepholes >= 4, "expected 1 shift->neg + 3 neg+add->sub fusions");
        assert!(
            !ssa.ops.iter().any(|op| matches!(*op, MOp::SubReg { rn: MReg::P(PReg::Xzr), .. })),
            "all single-use negs must be eliminated"
        );
        assert!(
            !ssa.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })),
            "chained shift must be eliminated"
        );
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::SubReg {
                dst,
                rn,
                rm,
                shift: Some(MShift::Lsr(3)),
                wide: true,
                sets_flags: false,
            } if dst == MReg::V(v_d1) && rn == MReg::V(v0) && rm == MReg::V(v1)
        )));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::SubReg { dst, rn, rm, shift: None, wide: true, sets_flags: false }
                if dst == MReg::V(v_d2) && rn == MReg::V(v_d1) && rm == MReg::V(v1)
        )));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::SubReg {
                dst,
                rn,
                rm,
                shift: Some(MShift::Lsr(2)),
                wide: true,
                sets_flags: false,
            } if dst == MReg::V(v_d3) && rn == MReg::V(v_d2) && rm == MReg::V(v1)
        )));

        // 2. Negative guards: multi-use `neg` AND clobbered `PReg` source MUST NOT fuse!
        let mut neg = new_ssa();
        let n0 = neg.alloc_vreg(VRegClass::Gpr64);
        let n1 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_multi = neg.alloc_vreg(VRegClass::Gpr64);
        let n_a = neg.alloc_vreg(VRegClass::Gpr64);
        let n_b = neg.alloc_vreg(VRegClass::Gpr64);
        let n_pneg = neg.alloc_vreg(VRegClass::Gpr64);
        let n_c = neg.alloc_vreg(VRegClass::Gpr64);
        let nb0 = neg.new_block(false);

        neg.begin_block(nb0);
        neg.push_inst(
            MOp::MovImm { dst: MReg::V(n0), imm: 100, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::V(n1), imm: 20, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        neg.push_inst(
            MOp::SubReg {
                dst: MReg::V(n_multi),
                rn: MReg::XZR,
                rm: MReg::V(n1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_a),
                rn: MReg::V(n0),
                rm: MReg::V(n_multi),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_b),
                rn: MReg::V(n_a),
                rm: MReg::V(n_multi),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::SubReg {
                dst: MReg::V(n_pneg),
                rn: MReg::XZR,
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X9),
                rn: MReg::p(PReg::X9),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_c),
                rn: MReg::V(n_b),
                rm: MReg::V(n_pneg),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(n_c), wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb0);

        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.peepholes, 0, "multi-use and clobbered-PReg negs must NOT fuse");
        assert_eq!(
            neg.ops
                .iter()
                .filter(|op| matches!(**op, MOp::SubReg { rn: MReg::P(PReg::Xzr), .. }))
                .count(),
            2
        );
    }

    #[test]
    fn mopt_fuses_shift_into_mvn_ssa_and_post_ra() {
        let alloc = test_alloc();

        // 1. Pre-RA SSA positive control: `lsr v_s, v0, #56; mvn v_d, v_s` -> `mvn v_d, v0, lsr #56`.
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_s = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_d = ssa.alloc_vreg(VRegClass::Gpr64);
        let sb0 = ssa.new_block(false);
        ssa.begin_block(sb0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 0xab00_0000_0000_0001u64, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::Ubfm { dst: MReg::V(v_s), rn: MReg::V(v0), immr: 56, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mvn { dst: MReg::V(v_d), rm: MReg::V(v_s), shift: None, wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_d), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(sb0);

        assert_eq!(mverify(&ssa), Ok(()));
        let ssa_stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(ssa_stats.peepholes >= 1, "SSA lsr + mvn must fuse");
        assert!(!ssa.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })));
        assert!(ssa.ops.iter().any(|op| matches!(
            *op,
            MOp::Mvn { dst, rm, shift: Some(MShift::Lsr(56)), wide: true }
                if dst == MReg::V(v_d) && rm == MReg::V(v0)
        )));

        // 2. Post-RA positive control: `lsr x9, x0, #56; mvn x9, x9; lsl x10, x1, #3; mvn x10, x10; add x0, x9, x10; ret`.
        let mut pos = new_post_ra();
        let pb0 = pos.new_block(false);
        pos.begin_block(pb0);
        pos.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X0), immr: 56, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mvn { dst: MReg::p(PReg::X9), rm: MReg::p(PReg::X9), shift: None, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Ubfm {
                dst: MReg::p(PReg::X10),
                rn: MReg::p(PReg::X1),
                immr: 61,
                imms: 60,
                wide: true,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::Mvn { dst: MReg::p(PReg::X10), rm: MReg::p(PReg::X10), shift: None, wide: true },
            MInstFlags::NONE,
        );
        pos.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X9),
                rm: MReg::p(PReg::X10),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        pos.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        pos.end_block(pb0);

        assert_eq!(mverify(&pos), Ok(()));
        let pos_stats = optimize_mfunc(&mut pos, alloc);
        assert_eq!(mverify(&pos), Ok(()));
        assert!(pos_stats.peepholes >= 2, "both post-RA shift + mvn pairs must fuse");
        assert!(!pos.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })));
        assert!(pos.ops.iter().any(|op| matches!(
            *op,
            MOp::Mvn {
                dst: MReg::P(PReg::X9),
                rm: MReg::P(PReg::X0),
                shift: Some(MShift::Lsr(56)),
                wide: true,
            }
        )));
        assert!(pos.ops.iter().any(|op| matches!(
            *op,
            MOp::Mvn {
                dst: MReg::P(PReg::X10),
                rm: MReg::P(PReg::X1),
                shift: Some(MShift::Lsl(3)),
                wide: true,
            }
        )));
        let pos_enc = mencode(&pos, alloc).expect("mencode shift-into-mvn must succeed");

        // 3. Post-RA negative guard: multi-use shift (`x9` read after `mvn`) MUST NOT fuse!
        let mut neg = new_post_ra();
        let nb0 = neg.new_block(false);
        neg.begin_block(nb0);
        neg.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X0), immr: 56, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mvn { dst: MReg::p(PReg::X10), rm: MReg::p(PReg::X9), shift: None, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X10),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb0);

        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.peepholes, 0, "multi-use shift must NOT fuse into mvn");
        assert!(neg.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })));
        let neg_enc = mencode(&neg, alloc).expect("mencode negative shift-into-mvn must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0, X1, X9, X10`.
            let f_pos: extern "C" fn(u64, u64) -> u64 = unsafe { pos_exec.as_fn() };
            let a = 0x5a00_0000_0000_0001u64;
            let b = 0x1234u64;
            assert_eq!(f_pos(a, b), (!(a >> 56)).wrapping_add(!(b << 3)));

            let neg_bytes = words_to_bytes(&neg_enc.words, alloc);
            let neg_exec =
                blaze_inproc::exec::Executable::new(&neg_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0, X9, X10`.
            let f_neg: extern "C" fn(u64) -> u64 = unsafe { neg_exec.as_fn() };
            assert_eq!(f_neg(a), u64::MAX);
        }
        let _ = (pos_enc, neg_enc);
    }

    #[test]
    fn mopt_fuses_post_ra_addsub_cbz_into_macro_fused_bcond() {
        let alloc = test_alloc();

        // 1. Positive post-RA control:
        //    b_entry: `add x2, x0, x1; cbz x2, b_exit` -> `adds x2, x0, x1; b.eq b_exit`
        //    b_loop:  `add x1, x1, #3; sub x0, x0, #1; cbnz x0, b_loop`
        //             -> `add x1, x1, #3; subs x0, x0, #1; b.ne b_loop`
        //    b_exit:  `mov x0, x1; ret`
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X2), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        func.begin_block(b_loop);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                imm12: 3,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbnz { rt: MReg::p(PReg::X0), target: b_loop, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats.peepholes, 2, "both add+cbz and sub+cbnz must fuse");
        assert!(!func.ops.iter().any(|op| matches!(*op, MOp::Cbz { .. } | MOp::Cbnz { .. })));

        let er = func.block_inst_range(b_entry);
        assert!(matches!(func.ops[er.start], MOp::AddReg { sets_flags: true, .. }));
        assert_eq!(func.ops[er.start + 1], MOp::BCond { cond: MCond::Eq, target: b_exit });
        assert_ne!(func.vflag_def[er.start], NO_VFLAG);
        assert_eq!(func.vflag_use[er.start + 1], func.vflag_def[er.start]);

        let lr = func.block_inst_range(b_loop);
        assert!(matches!(func.ops[lr.start + 1], MOp::SubImm { sets_flags: true, .. }));
        assert_eq!(func.ops[lr.start + 2], MOp::BCond { cond: MCond::Ne, target: b_loop });
        assert_ne!(func.vflag_def[lr.start + 1], NO_VFLAG);
        assert_eq!(func.vflag_use[lr.start + 2], func.vflag_def[lr.start + 1]);

        let enc_out = mencode(&func, alloc).expect("mencode macro-fused subs+b.ne must succeed");

        // Second pass must be strictly idempotent.
        let stats2 = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats2.peepholes, 0);

        // 2. Negative guards: live vflag after `cbz`, `rn == SP`, and width mismatch MUST NOT fuse!
        let mut neg = new_post_ra();
        let nb0 = neg.new_block(false);
        let nb1 = neg.new_block(false);
        let nb2 = neg.new_block(false);
        let n_fall = neg.new_block(false);
        let nexit = neg.new_block(false);

        neg.begin_block(nb0);
        let cmp = neg.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X1), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        neg.set_vflag_def(cmp, 0);
        neg.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: nexit, wide: true },
            MInstFlags::TERMINATOR,
        );
        let blt = neg.push_inst(
            MOp::BCond { cond: MCond::Lt, target: nb2 },
            MInstFlags::TERMINATOR,
        );
        neg.set_vflag_use(blt, 0);
        neg.end_block(nb0);

        neg.begin_block(nb1);
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::SP,
                imm12: 8,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X2), target: nexit, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.end_block(nb1);

        neg.begin_block(nb2);
        neg.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: nexit, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.end_block(nb2);

        neg.begin_block(n_fall);
        neg.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 7, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_fall);

        neg.begin_block(nexit);
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nexit);

        neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.peepholes, 0, "live-vflag, SP-rn, and width-mismatch must NOT fuse");
        assert_eq!(neg.ops.iter().filter(|op| matches!(**op, MOp::Cbz { .. })).count(), 3);
        let _neg_enc = mencode(&neg, alloc).expect("mencode guarded cbz must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X2`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(0, 0), 0);
            assert_eq!(f(4, 10), 22);
            assert_eq!(f(1, 5), 8);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_fuses_cset_add_into_csinc_ssa() {
        let alloc = test_alloc();

        // 1. SSA positive controls:
        //    (a) `cmp v0, v1 [vf0]; cset v_c1 (Ne) [vf0]; add v_d1, v_acc, v_c1`
        //        -> `csinc v_d1, v_acc, v_acc, Ne [vf0]`
        //    (b) `cmp v0, v1 [vf1]; cset w_c2 (Hs) [vf1]; add v_other, v_d1, #10; add v_d2, v_c2, v_other`
        //        -> `add v_other, v_d1, #10; csinc v_d2, v_other, v_other, Hs [vf1]`
        let mut ssa = new_ssa();
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_acc = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_c1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_d1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_c2 = ssa.alloc_vreg(VRegClass::Gpr32);
        let v_other = ssa.alloc_vreg(VRegClass::Gpr64);
        let v_d2 = ssa.alloc_vreg(VRegClass::Gpr64);
        let b0 = ssa.new_block(false);

        ssa.begin_block(b0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 0x100, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v1), imm: 0x200, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v_acc), imm: 0x300, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        let cmp0 = ssa.push_inst(
            MOp::CmpReg { rn: MReg::V(v0), rm: MReg::V(v1), shift: None, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(cmp0, 0);
        let cset0 = ssa.push_inst(
            MOp::Csinc {
                dst: MReg::V(v_c1),
                rn: MReg::XZR,
                rm: MReg::XZR,
                cond: MCond::Ne,
                wide: true,
            },
            MInstFlags::NONE,
        );
        ssa.set_vflag_use(cset0, 0);
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_d1),
                rn: MReg::V(v_acc),
                rm: MReg::V(v_c1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp1 = ssa.push_inst(
            MOp::CmpReg { rn: MReg::V(v0), rm: MReg::V(v1), shift: None, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(cmp1, 1);
        let cset1 = ssa.push_inst(
            MOp::Csinc {
                dst: MReg::V(v_c2),
                rn: MReg::XZR,
                rm: MReg::XZR,
                cond: MCond::Hs,
                wide: false,
            },
            MInstFlags::NONE,
        );
        ssa.set_vflag_use(cset1, 1);
        ssa.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_other),
                rn: MReg::V(v_d1),
                imm12: 10,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_d2),
                rn: MReg::V(v_c2),
                rm: MReg::V(v_other),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ssa.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_d2), wide: true },
            MInstFlags::NONE,
        );
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(b0);

        assert_eq!(mverify(&ssa), Ok(()));
        let stats = optimize_mfunc(&mut ssa, alloc);
        assert_eq!(mverify(&ssa), Ok(()));
        assert!(stats.peepholes >= 2, "both cset + add pairs must fuse into csinc");
        assert!(
            !ssa.ops.iter().any(|op| matches!(
                *op,
                MOp::Csinc { rn: MReg::P(PReg::Xzr), rm: MReg::P(PReg::Xzr), .. }
            )),
            "both cset ops must be eliminated"
        );
        let mut saw_cinc0 = false;
        let mut saw_cinc1 = false;
        for (idx, op) in ssa.ops.iter().enumerate() {
            if matches!(
                *op,
                MOp::Csinc { dst, rn, rm, cond: MCond::Ne, wide: true }
                    if dst == MReg::V(v_d1) && rn == MReg::V(v_acc) && rm == MReg::V(v_acc)
            ) {
                assert_eq!(ssa.vflag_use[idx], 0);
                saw_cinc0 = true;
            }
            if matches!(
                *op,
                MOp::Csinc { dst, rn, rm, cond: MCond::Hs, wide: true }
                    if dst == MReg::V(v_d2) && rn == MReg::V(v_other) && rm == MReg::V(v_other)
            ) {
                assert_eq!(ssa.vflag_use[idx], 1);
                saw_cinc1 = true;
            }
        }
        assert!(saw_cinc0 && saw_cinc1, "expected both fused Csinc instructions with intact vflags");

        // 2. Negative guards: intervening flag write AND multi-use `cset` MUST NOT fuse!
        let mut neg = new_ssa();
        let n0 = neg.alloc_vreg(VRegClass::Gpr64);
        let n1 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_c_clob = neg.alloc_vreg(VRegClass::Gpr64);
        let n_r1 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_c_multi = neg.alloc_vreg(VRegClass::Gpr64);
        let n_r2 = neg.alloc_vreg(VRegClass::Gpr64);
        let n_r3 = neg.alloc_vreg(VRegClass::Gpr64);
        let nb0 = neg.new_block(false);

        neg.begin_block(nb0);
        neg.push_inst(
            MOp::MovImm { dst: MReg::V(n0), imm: 10, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::V(n1), imm: 20, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        let nc0 = neg.push_inst(
            MOp::CmpReg { rn: MReg::V(n0), rm: MReg::V(n1), shift: None, wide: true },
            MInstFlags::NONE,
        );
        neg.set_vflag_def(nc0, 0);
        let ncs0 = neg.push_inst(
            MOp::Csinc {
                dst: MReg::V(n_c_clob),
                rn: MReg::XZR,
                rm: MReg::XZR,
                cond: MCond::Ne,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg.set_vflag_use(ncs0, 0);
        // Intervening `CmpImm` clobbers NZCV -> blocks sinking `cset` into `add`!
        let nc1 = neg.push_inst(
            MOp::CmpImm { rn: MReg::V(n0), imm12: 5, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        neg.set_vflag_def(nc1, 1);
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_r1),
                rn: MReg::V(n1),
                rm: MReg::V(n_c_clob),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let ncs1 = neg.push_inst(
            MOp::Csinc {
                dst: MReg::V(n_c_multi),
                rn: MReg::XZR,
                rm: MReg::XZR,
                cond: MCond::Ne,
                wide: true,
            },
            MInstFlags::NONE,
        );
        neg.set_vflag_use(ncs1, 1);
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_r2),
                rn: MReg::V(n_r1),
                rm: MReg::V(n_c_multi),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddReg {
                dst: MReg::V(n_r3),
                rn: MReg::V(n_r2),
                rm: MReg::V(n_c_multi),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(n_r3), wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb0);

        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.peepholes, 0, "clobbered-flag and multi-use cset must NOT fuse");
        assert_eq!(
            neg.ops
                .iter()
                .filter(|op| matches!(
                    **op,
                    MOp::Csinc { rn: MReg::P(PReg::Xzr), rm: MReg::P(PReg::Xzr), .. }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn mopt_relocates_short_edge_split_and_prunes_unreachable_empty_blocks() {
        let alloc = test_alloc();

        // 1. Positive post-RA control:
        //    Layout: `[b_entry, b_fast, b_dead_empty, b_dead_tramp, b_t, b_s]`
        //    - `b_dead_empty` (0 insts) and `b_dead_tramp` (`b b_t`) are unreachable -> pruned!
        //    - `b_s` (`mov x0, #42; b b_t`) has `PrevS = b_t` (`ret`) and `P = b_fast` (`ret`)
        //      -> relocated right before `b_t`, trailing `b b_t` deleted into 0-inst fallthrough!
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_fast = func.new_block(false);
        let b_dead_empty = func.new_block(false);
        let b_dead_tramp = func.new_block(false);
        let b_t = func.new_block(false);
        let b_s = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: b_s, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        func.begin_block(b_fast);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_fast);

        func.begin_block(b_dead_empty);
        func.end_block(b_dead_empty);

        func.begin_block(b_dead_tramp);
        func.push_inst(MOp::B { target: b_t }, MInstFlags::TERMINATOR);
        func.end_block(b_dead_tramp);

        func.begin_block(b_t);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 7,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_t);

        func.begin_block(b_s);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 42, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_t }, MInstFlags::TERMINATOR);
        func.end_block(b_s);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.branches_folded >= 2, "expected dead trampoline + relocated B folding");
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_fast, b_s, b_t],
            "unreachable empty blocks pruned and b_s relocated right before b_t"
        );
        assert!(
            !func.ops.iter().any(|op| matches!(*op, MOp::B { .. })),
            "all unconditional B instructions must be folded into fallthrough"
        );
        assert_eq!(func.block_inst_range(b_s).len(), 1);
        let pos_enc = mencode(&func, alloc).expect("mencode relocated block must succeed");

        // 2. Negative guard 1: `PrevS` (`b_entry`) falls through into `b_s` -> MUST NOT relocate!
        let mut neg_ft = new_post_ra();
        let n_entry = neg_ft.new_block(false);
        let n_s = neg_ft.new_block(false);
        let n_other = neg_ft.new_block(false);
        let n_t = neg_ft.new_block(false);

        neg_ft.begin_block(n_entry);
        neg_ft.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: n_other, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg_ft.end_block(n_entry);

        neg_ft.begin_block(n_s);
        neg_ft.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 10, wide: true },
            MInstFlags::NONE,
        );
        neg_ft.push_inst(MOp::B { target: n_t }, MInstFlags::TERMINATOR);
        neg_ft.end_block(n_s);

        neg_ft.begin_block(n_other);
        neg_ft.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 20, wide: true },
            MInstFlags::NONE,
        );
        neg_ft.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_ft.end_block(n_other);

        neg_ft.begin_block(n_t);
        neg_ft.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg_ft.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_ft.end_block(n_t);

        neg_ft.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg_ft), Ok(()));
        let _ = optimize_mfunc(&mut neg_ft, alloc);
        assert_eq!(mverify(&neg_ft), Ok(()));
        assert_eq!(
            neg_ft.block_order.as_slice(),
            &[n_entry, n_s, n_other, n_t],
            "fallthrough predecessor PrevS must prevent relocating b_s"
        );
        let neg_ft_enc = mencode(&neg_ft, alloc).expect("mencode guarded fallthrough must succeed");

        // 3. Negative guard 2: jump table (`AdrTable`) present -> MUST NOT prune unreachable block!
        let mut neg_jt = new_post_ra();
        let jt_entry = neg_jt.new_block(false);
        let jt_empty = neg_jt.new_block(false);
        let jt_exit = neg_jt.new_block(false);

        neg_jt.begin_block(jt_entry);
        neg_jt.push_inst(
            MOp::AdrTable { dst: MReg::p(PReg::X1), table_id: 0 },
            MInstFlags::NONE,
        );
        neg_jt.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_jt.end_block(jt_entry);

        neg_jt.begin_block(jt_empty);
        neg_jt.end_block(jt_empty);

        neg_jt.begin_block(jt_exit);
        neg_jt.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg_jt.end_block(jt_exit);

        neg_jt.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg_jt), Ok(()));
        let _ = optimize_mfunc(&mut neg_jt, alloc);
        assert_eq!(mverify(&neg_jt), Ok(()));
        assert_eq!(
            neg_jt.block_order.as_slice(),
            &[jt_entry, jt_empty, jt_exit],
            "AdrTable presence must skip unreachable-block pruning"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`.
            let f_pos: extern "C" fn(u64) -> u64 = unsafe { pos_exec.as_fn() };
            assert_eq!(f_pos(0), 49);
            assert_eq!(f_pos(10), 11);

            let neg_bytes = words_to_bytes(&neg_ft_enc.words, alloc);
            let neg_exec =
                blaze_inproc::exec::Executable::new(&neg_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`.
            let f_neg: extern "C" fn(u64) -> u64 = unsafe { neg_exec.as_fn() };
            assert_eq!(f_neg(0), 20);
            assert_eq!(f_neg(1), 11);
        }
        let _ = (pos_enc, neg_ft_enc);
    }

    #[test]
    fn mopt_branch_opt_skips_pruning_when_unwind_note_present() {
        let alloc = test_alloc();
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_landing_empty = func.new_block(false);
        let b_landing_tramp = func.new_block(false);
        let b_other = func.new_block(false);
        let b_cleanup = func.new_block(false);

        func.begin_block(b_entry);
        let i_bl = func.push_inst(MOp::Bl, MInstFlags::CALL);
        func.attach_note(i_bl, MNoteKind::Unwind { unwind_idx: 0 });
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_entry);

        // Unreachable empty and trampoline landing pads referenced only via unwind metadata.
        func.begin_block(b_landing_empty);
        func.end_block(b_landing_empty);

        func.begin_block(b_landing_tramp);
        func.push_inst(MOp::B { target: b_cleanup }, MInstFlags::TERMINATOR);
        func.end_block(b_landing_tramp);

        func.begin_block(b_other);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_other);

        func.begin_block(b_cleanup);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 1, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_cleanup);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let _ = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_landing_empty, b_landing_tramp, b_other, b_cleanup],
            "MNoteKind::Unwind presence must skip Pass 4c unreachable-block pruning"
        );
        let enc = mencode(&func, alloc).expect("mencode with unwind note must succeed");
        assert_eq!(enc.unwind_byte_offs.len(), 1);
    }

    #[test]
    fn mopt_branch_opt_colocates_secondary_edge_split_before_fallthrough_p() {
        let alloc = test_alloc();

        // 1. Positive case:
        //    Layout: `[b_entry, b_pp, b_p, b_t, b_cold_exit, b_s]`
        //    - `b_p` is a 1-inst pure fallthrough copy block into `b_t`.
        //    - `b_pp` (`PP`) ends with unconditional `B b_cold_exit`.
        //    - `b_s` (`S -> B b_t`) sits at the cold tail (`s_pos = 5 > t_pos = 3`), preceded by
        //      unconditional `b_cold_exit` (`Ret`).
        //    -> Pass 4d co-locates `b_s` to `t_pos - 1` (immediately before `b_p`), keeping
        //       `b_s`'s `B b_t` intact so `b_s` shares `b_t`'s I-cache line.
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_pp = func.new_block(false);
        let b_p = func.new_block(false);
        let b_t = func.new_block(false);
        let b_cold_exit = func.new_block(false);
        let b_s = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: b_p, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        func.begin_block(b_pp);
        let c_pp = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 1, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c_pp, 0);
        let br_pp = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_s },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_pp, 0);
        func.push_inst(MOp::B { target: b_cold_exit }, MInstFlags::TERMINATOR);
        func.end_block(b_pp);

        func.begin_block(b_p);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 100, wide: true },
            MInstFlags::NONE,
        );
        func.end_block(b_p);

        func.begin_block(b_t);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 7,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_t);

        func.begin_block(b_cold_exit);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 20,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_cold_exit);

        func.begin_block(b_s);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 200, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_t }, MInstFlags::TERMINATOR);
        func.end_block(b_s);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let _ = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_pp, b_s, b_p, b_t, b_cold_exit],
            "secondary short edge-split block b_s must relocate immediately before pure fallthrough b_p"
        );
        let s_range = func.block_inst_range(b_s);
        assert_eq!(s_range.len(), 2, "secondary co-located b_s must keep MovImm + B b_t");
        assert!(
            matches!(func.ops[s_range.start + 1], MOp::B { target } if target == b_t),
            "secondary co-located b_s must keep trailing B b_t intact"
        );
        let pos_enc = mencode(&func, alloc).expect("mencode secondary co-located block must succeed");

        // 2. Negative guard: `P` ends with a conditional branch (`Cbz`) -> MUST NOT co-locate!
        let mut neg = new_post_ra();
        let n_entry = neg.new_block(false);
        let n_pp = neg.new_block(false);
        let n_p = neg.new_block(false);
        let n_t = neg.new_block(false);
        let n_cold_exit = neg.new_block(false);
        let n_s = neg.new_block(false);

        neg.begin_block(n_entry);
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: n_p, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.end_block(n_entry);

        neg.begin_block(n_pp);
        let c_npp = neg.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 1, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        neg.set_vflag_def(c_npp, 0);
        let br_npp = neg.push_inst(
            MOp::BCond { cond: MCond::Eq, target: n_s },
            MInstFlags::TERMINATOR,
        );
        neg.set_vflag_use(br_npp, 0);
        neg.push_inst(MOp::B { target: n_cold_exit }, MInstFlags::TERMINATOR);
        neg.end_block(n_pp);

        neg.begin_block(n_p);
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X1), target: n_cold_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.end_block(n_p);

        neg.begin_block(n_t);
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 7,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_t);

        neg.begin_block(n_cold_exit);
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 20,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_cold_exit);

        neg.begin_block(n_s);
        neg.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 200, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::B { target: n_t }, MInstFlags::TERMINATOR);
        neg.end_block(n_s);

        neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let _ = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(
            neg.block_order.as_slice(),
            &[n_entry, n_pp, n_p, n_t, n_cold_exit, n_s],
            "conditional-branch P must prevent secondary co-location of n_s"
        );
        let neg_enc = mencode(&neg, alloc).expect("mencode guarded secondary must succeed");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`.
            let f_pos: extern "C" fn(u64) -> u64 = unsafe { pos_exec.as_fn() };
            assert_eq!(f_pos(0), 107);
            assert_eq!(f_pos(1), 207);
            assert_eq!(f_pos(5), 25);

            let neg_bytes = words_to_bytes(&neg_enc.words, alloc);
            let neg_exec =
                blaze_inproc::exec::Executable::new(&neg_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0` and `X1`.
            let f_neg: extern "C" fn(u64, u64) -> u64 = unsafe { neg_exec.as_fn() };
            assert_eq!(f_neg(0, 0), 20);
            assert_eq!(f_neg(0, 1), 7);
            assert_eq!(f_neg(1, 0), 207);
            assert_eq!(f_neg(5, 0), 25);
        }
        let _ = (pos_enc, neg_enc);
    }

    #[test]
    fn mopt_post_ra_tail_merges_identical_short_blocks() {
        let alloc = test_alloc();

        // 1. Positive post-RA control (`lz77_scan_default_leaf` false-match tail pattern):
        //    4 identical 3-inst blocks (`add x2, #1; add x0, #1; b b_hdr`) with `b_dup2`
        //    immediately preceding `b_hdr` -> merged into canonical `b_dup2` (falling straight
        //    through into `b_hdr`), all 3 duplicates pruned from `block_order`!
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_c0 = func.new_block(false);
        let b_c1 = func.new_block(false);
        let b_dup0 = func.new_block(false);
        let b_dup1 = func.new_block(false);
        let b_dup2 = func.new_block(false);
        let b_hdr = func.new_block(false);
        let b_dup3 = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X2), imm: 10, wide: true },
            MInstFlags::NONE,
        );
        let c_e = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c_e, 0);
        let br_e = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_dup0 },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_e, 0);
        func.end_block(b_entry);

        func.begin_block(b_c0);
        let c_0 = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 1, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c_0, 0);
        let br_0 = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_dup1 },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_0, 0);
        func.end_block(b_c0);

        func.begin_block(b_c1);
        let c_1 = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 2, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c_1, 0);
        let br_1 = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_dup3 },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_1, 0);
        func.push_inst(MOp::B { target: b_dup2 }, MInstFlags::TERMINATOR);
        func.end_block(b_c1);

        for dup in [b_dup0, b_dup1, b_dup2] {
            func.begin_block(dup);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::p(PReg::X2),
                    rn: MReg::p(PReg::X2),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::p(PReg::X0),
                    rn: MReg::p(PReg::X0),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::B { target: b_hdr }, MInstFlags::TERMINATOR);
            func.end_block(dup);
        }

        func.begin_block(b_hdr);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
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
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_hdr);

        func.begin_block(b_dup3);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_hdr }, MInstFlags::TERMINATOR);
        func.end_block(b_dup3);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.branches_folded >= 3, "3 duplicate tails must merge into canonical b_dup2");
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_c0, b_c1, b_dup2, b_hdr],
            "b_dup0, b_dup1, and b_dup3 must be merged into b_dup2 and pruned"
        );
        assert_eq!(func.block_inst_range(b_dup2).len(), 2, "b_dup2 falls straight into b_hdr");
        assert!(
            !func.ops.iter().any(|op| matches!(op.branch_target(), Some(t) if t == b_dup0 || t == b_dup1 || t == b_dup3)),
            "no branch may still target a pruned duplicate"
        );
        let pos_enc = mencode(&func, alloc).expect("mencode tail-merged func must succeed");

        // 2. Negative guard: jump-table anchor (`AdrTable`) present -> MUST NOT merge or prune!
        let mut neg = new_post_ra();
        let n_entry = neg.new_block(false);
        let n_d0 = neg.new_block(false);
        let n_d1 = neg.new_block(false);
        let n_hdr = neg.new_block(false);

        neg.begin_block(n_entry);
        neg.push_inst(
            MOp::AdrTable { dst: MReg::p(PReg::X3), table_id: 0 },
            MInstFlags::PINNED,
        );
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: n_d0, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.push_inst(MOp::B { target: n_d1 }, MInstFlags::TERMINATOR);
        neg.end_block(n_entry);

        for d in [n_d0, n_d1] {
            neg.begin_block(d);
            neg.push_inst(
                MOp::AddImm {
                    dst: MReg::p(PReg::X0),
                    rn: MReg::p(PReg::X0),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            neg.push_inst(MOp::B { target: n_hdr }, MInstFlags::TERMINATOR);
            neg.end_block(d);
        }

        neg.begin_block(n_hdr);
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_hdr);

        neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let _ = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(
            neg.block_order.as_slice(),
            &[n_entry, n_d0, n_d1, n_hdr],
            "AdrTable presence must skip Pass 4e tail merging"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X2`.
            let f_pos: extern "C" fn(u64, u64) -> u64 = unsafe { pos_exec.as_fn() };
            assert_eq!(f_pos(0, 100), 112);
            assert_eq!(f_pos(1, 100), 113);
            assert_eq!(f_pos(2, 100), 114);
            assert_eq!(f_pos(5, 100), 117);
        }
        let _ = pos_enc;
    }

    #[test]
    fn mopt_post_ra_relocates_hot_loop_header_and_prev_before_body() {
        let alloc = test_alloc();

        // 1. Positive post-RA control (`lz77_scan_default_leaf` outer header pattern):
        //    Layout: `[b_entry, b_body, b_cold_exit, b_prev_h, b_hdr]`
        //    - `b_entry` ends with `b b_hdr` (uncond -> `b_body` has NO fallthrough pred).
        //    - `b_body` is a 7-inst loop body ending in `b b_prev_h`.
        //    - `b_cold_exit` ends with `ret` (uncond -> `[b_prev_h, b_hdr]` is fallthrough-free).
        //    - `b_prev_h` is a 2-inst pure ALU fallthrough block (`add x3, #1; add x0, #1`).
        //    - `b_hdr` is a 5-inst displaced header (`lsr; add; cmp; b.ls b_body; b b_cold_exit`).
        //    -> Pass 4f relocates `[b_prev_h, b_hdr]` immediately before `b_body` and inverts
        //       `b_hdr` to `b.hi b_cold_exit` falling straight through into `b_body`!
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_body = func.new_block(false);
        let b_cold_exit = func.new_block(false);
        let b_prev_h = func.new_block(false);
        let b_hdr = func.new_block(true);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X3), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_hdr }, MInstFlags::TERMINATOR);
        func.end_block(b_entry);

        func.begin_block(b_body);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X0),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::LogicalReg {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X0),
                shift: None,
                invert_rm: false,
                kind: LogicalKind::Eor,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X5),
                imm12: 3,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X5),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_prev_h }, MInstFlags::TERMINATOR);
        func.end_block(b_body);

        func.begin_block(b_cold_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_cold_exit);

        func.begin_block(b_prev_h);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X3),
                rn: MReg::p(PReg::X3),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.end_block(b_prev_h);

        func.begin_block(b_hdr);
        func.push_inst(
            MOp::Ubfm { dst: MReg::p(PReg::X7), rn: MReg::p(PReg::X0), immr: 1, imms: 63, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X6),
                rn: MReg::p(PReg::X3),
                rm: MReg::p(PReg::X7),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X6),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let bls = func.push_inst(
            MOp::BCond { cond: MCond::Ls, target: b_body },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(bls, 0);
        func.push_inst(MOp::B { target: b_cold_exit }, MInstFlags::TERMINATOR);
        func.end_block(b_hdr);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.branches_folded >= 1, "Pass 4f must relocate and invert displaced header");
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_prev_h, b_hdr, b_body, b_cold_exit],
            "[b_prev_h, b_hdr] must move together immediately before b_body"
        );
        let hdr_r = func.block_inst_range(b_hdr);
        assert_eq!(
            func.ops[hdr_r.end - 1],
            MOp::BCond { cond: MCond::Hi, target: b_cold_exit },
            "b_hdr must invert b.ls b_body; b b_cold_exit -> b.hi b_cold_exit (fallthrough to b_body)"
        );
        assert_eq!(func.vflag_use[hdr_r.end - 1], 0, "vflag_use must remain intact after inversion");
        assert!(
            !func.ops[hdr_r].iter().any(|op| matches!(*op, MOp::B { .. })),
            "b_hdr must have zero trailing B after inversion"
        );
        let pos_enc = mencode(&func, alloc).expect("mencode relocated hot loop header must succeed");

        // 2. Negative guard: `b_entry` falls through into `b_body` -> `b_hdr` MUST NOT relocate!
        let mut neg = new_post_ra();
        let n_entry = neg.new_block(false);
        let n_body = neg.new_block(false);
        let n_exit = neg.new_block(false);
        let n_hdr = neg.new_block(true);

        neg.begin_block(n_entry);
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: n_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.end_block(n_entry);

        neg.begin_block(n_body);
        neg.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::B { target: n_hdr }, MInstFlags::TERMINATOR);
        neg.end_block(n_body);

        neg.begin_block(n_exit);
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_exit);

        neg.begin_block(n_hdr);
        let n_cmp = neg.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        neg.set_vflag_def(n_cmp, 0);
        let n_bne = neg.push_inst(
            MOp::BCond { cond: MCond::Ne, target: n_body },
            MInstFlags::TERMINATOR,
        );
        neg.set_vflag_use(n_bne, 0);
        neg.push_inst(MOp::B { target: n_exit }, MInstFlags::TERMINATOR);
        neg.end_block(n_hdr);

        neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let _ = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(
            &neg.block_order.as_slice()[0..2],
            &[n_entry, n_body],
            "fallthrough into n_body must prevent relocating n_hdr before n_body"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X7`.
            let f_pos: extern "C" fn(u64, u64, u64) -> u64 = unsafe { pos_exec.as_fn() };
            assert_eq!(f_pos(0, 10, 2), 60);
            assert_eq!(f_pos(10, 7, 2), 7);
        }
        let _ = pos_enc;
    }

    #[test]
    fn mopt_post_ra_inverts_and_colocates_distant_backedge_trampoline() {
        let alloc = test_alloc();

        // `build_huffman_weights` pattern:
        // Layout: `[b_entry, b_hdr, b_exit, b_cold_pad, b_split]`
        // - `b_entry` falls through into `b_hdr` (so Phases 1 & 2 cannot relocate `b_split` before `b_hdr`).
        // - `b_hdr` ends in single `b.lo b_split` (falling through to `b_exit`).
        // - `b_split` (`mov x14, x13; b b_hdr`) sits in the cold tail after uncond `b_cold_pad`,
        //   has 1 pred (`b_hdr`), and branches back to `b_hdr` (`pos[b_hdr] <= pos[b_hdr]`).
        // -> Pass 4d Phase 3 inverts `b_hdr` to `b.hs b_exit` and co-locates `b_split` at `p_pos + 1`!
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_hdr = func.new_block(true);
        let b_exit = func.new_block(false);
        let b_cold_pad = func.new_block(false);
        let b_split = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X14), imm: 0, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X13), imm: 1, wide: true },
            MInstFlags::NONE,
        );
        func.end_block(b_entry);

        func.begin_block(b_hdr);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X13),
                rn: MReg::p(PReg::X13),
                imm12: 2,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X13),
                rm: MReg::p(PReg::X0),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let blo = func.push_inst(
            MOp::BCond { cond: MCond::Lo, target: b_split },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(blo, 0);
        func.end_block(b_hdr);

        func.begin_block(b_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X14), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.begin_block(b_cold_pad);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 999, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_cold_pad);

        func.begin_block(b_split);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X14), src: MReg::p(PReg::X13), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_hdr }, MInstFlags::TERMINATOR);
        func.end_block(b_split);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.branches_folded >= 1, "Pass 4d Phase 3 must invert & co-locate back-edge split");
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_hdr, b_split, b_exit, b_cold_pad],
            "b_split must co-locate immediately after b_hdr"
        );
        let hdr_r = func.block_inst_range(b_hdr);
        assert_eq!(
            func.ops[hdr_r.end - 1],
            MOp::BCond { cond: MCond::Hs, target: b_exit },
            "b_hdr must invert b.lo b_split -> b.hs b_exit"
        );
        assert_eq!(func.vflag_use[hdr_r.end - 1], 0);
        let split_r = func.block_inst_range(b_split);
        assert_eq!(split_r.len(), 2);
        assert_eq!(func.ops[split_r.start + 1], MOp::B { target: b_hdr });

        // Second pass must be strictly idempotent (no ping-pong!).
        let stats2 = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats2.branches_folded, 0);
        assert_eq!(func.block_order.as_slice(), &[b_entry, b_hdr, b_split, b_exit, b_cold_pad]);

        let enc_out = mencode(&func, alloc).expect("mencode co-located back-edge split must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0, X13, X14`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(0), 0);
            assert_eq!(f(4), 3);
            assert_eq!(f(10), 9);
        }
        let _ = enc_out;
    }

    #[test]
    fn mopt_post_ra_fuses_stp_ldp_q128_post_index() {
        let alloc = test_alloc();

        // 1. Positive Q128 `Stp` post-index (`stp q16, q16, [x14], #32`):
        //    Signature: `fn(dst: *mut u64, fill_src: *const u64, iters: u64) -> *mut u64`.
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        func.begin_block(b_entry);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X14), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Ldr {
                dst: MReg::vec(16),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X1), off: 0 },
                size: MemSize::Q128,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X2), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        func.begin_block(b_loop);
        func.push_inst(
            MOp::Stp {
                src0: MReg::vec(16),
                src1: MReg::vec(16),
                base: MReg::p(PReg::X14),
                off: 0,
                size: MemSize::Q128,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_WRITE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X14),
                rn: MReg::p(PReg::X14),
                imm12: 32,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbnz { rt: MReg::p(PReg::X2), target: b_loop, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X14), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(stats.post_idx_formed, 1, "stp q16, q16, [x14] + add #32 must fuse");
        assert!(func.ops.iter().any(|op| matches!(
            *op,
            MOp::Stp {
                src0: MReg::P(PReg::V(16)),
                src1: MReg::P(PReg::V(16)),
                base: MReg::P(PReg::X14),
                off: 32,
                size: MemSize::Q128,
                post_idx: true,
                pre_idx: false,
            }
        )));
        assert!(
            !func.ops.iter().any(|op| matches!(
                *op,
                MOp::AddImm { dst: MReg::P(PReg::X14), rn: MReg::P(PReg::X14), .. }
            )),
            "folded AddImm on x14 must be removed"
        );
        let pos_enc = mencode(&func, alloc).expect("mencode Q128 stp post-index must succeed");

        // 2. Positive Q128 `Ldp` negative post-index (`ldp q17, q18, [x15], #-32`) across >12
        //    intervening unrelated ALU ops (exercises widened `POST_IDX_WINDOW`).
        let mut ldp_fn = new_post_ra();
        let lb0 = ldp_fn.new_block(false);
        ldp_fn.begin_block(lb0);
        ldp_fn.push_inst(
            MOp::Ldp {
                dst0: MReg::vec(17),
                dst1: MReg::vec(18),
                base: MReg::p(PReg::X15),
                off: 0,
                size: MemSize::Q128,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_READ,
        );
        for _ in 0..14 {
            ldp_fn.push_inst(
                MOp::AddImm {
                    dst: MReg::p(PReg::X9),
                    rn: MReg::p(PReg::X9),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        ldp_fn.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X15),
                rn: MReg::p(PReg::X15),
                imm12: 32,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        ldp_fn.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X15), wide: true },
            MInstFlags::NONE,
        );
        ldp_fn.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ldp_fn.end_block(lb0);

        assert_eq!(mverify(&ldp_fn), Ok(()));
        let ldp_stats = optimize_mfunc(&mut ldp_fn, alloc);
        assert_eq!(mverify(&ldp_fn), Ok(()));
        assert_eq!(ldp_stats.post_idx_formed, 1, "ldp q17, q18 + sub #32 across 14 ops must fuse");
        assert!(ldp_fn.ops.iter().any(|op| matches!(
            *op,
            MOp::Ldp {
                dst0: MReg::P(PReg::V(17)),
                dst1: MReg::P(PReg::V(18)),
                base: MReg::P(PReg::X15),
                off: -32,
                size: MemSize::Q128,
                post_idx: true,
                pre_idx: false,
            }
        )));
        let _ldp_enc = mencode(&ldp_fn, alloc).expect("mencode Q128 ldp post-index must succeed");

        // 3. Negative guards: out-of-range (+1024), unaligned (+24), `SP` base, and `dst0 == base`
        //    MUST NOT fuse!
        let mut neg = new_post_ra();
        let nb0 = neg.new_block(false);
        neg.begin_block(nb0);
        // Out of Q128 7-bit scaled range (max +1008):
        neg.push_inst(
            MOp::Stp {
                src0: MReg::vec(16),
                src1: MReg::vec(16),
                base: MReg::p(PReg::X14),
                off: 0,
                size: MemSize::Q128,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_WRITE,
        );
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X14),
                rn: MReg::p(PReg::X14),
                imm12: 1024,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // Unaligned for Q128 (24 % 16 != 0):
        neg.push_inst(
            MOp::Stp {
                src0: MReg::vec(16),
                src1: MReg::vec(16),
                base: MReg::p(PReg::X13),
                off: 0,
                size: MemSize::Q128,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_WRITE,
        );
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X13),
                rn: MReg::p(PReg::X13),
                imm12: 24,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // `base == SP`:
        neg.push_inst(
            MOp::Stp {
                src0: MReg::p(PReg::X3),
                src1: MReg::p(PReg::X4),
                base: MReg::SP,
                off: 0,
                size: MemSize::X64,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_WRITE,
        );
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: 16,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        // `Ldp` with `dst0 == base`:
        neg.push_inst(
            MOp::Ldp {
                dst0: MReg::p(PReg::X12),
                dst1: MReg::p(PReg::X11),
                base: MReg::p(PReg::X12),
                off: 0,
                size: MemSize::X64,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_READ,
        );
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X12),
                rn: MReg::p(PReg::X12),
                imm12: 16,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(nb0);

        assert_eq!(mverify(&neg), Ok(()));
        let neg_stats = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(neg_stats.post_idx_formed, 0, "all 4 guarded pair post-index cases must reject");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..X2, X14, V16`.
            let f_pos: extern "C" fn(*mut u64, *const u64, u64) -> *mut u64 =
                unsafe { pos_exec.as_fn() };
            let fill = [0x1122_3344_5566_7788u64, 0x99aa_bbcc_ddee_ff00u64];
            let mut dst = [0u64; 8];
            let end_ptr = f_pos(dst.as_mut_ptr(), fill.as_ptr(), 2);
            // SAFETY: `dst` has length 8, so `.add(8)` is the exact one-past-end pointer.
            assert_eq!(end_ptr, unsafe { dst.as_mut_ptr().add(8) });
            assert_eq!(dst, [fill[0], fill[1], fill[0], fill[1], fill[0], fill[1], fill[0], fill[1]]);
        }
        let _ = pos_enc;
    }

    #[test]
    fn mopt_post_ra_tail_merges_and_hoists_long_copy_trampolines() {
        let alloc = test_alloc();

        // 1. Combined 6-inst `Mov`/`MovImm` + `B T` tail merge (Pass 4e, widened to 12 ops) AND
        //    post-`Ret` copy-trampoline hoist (Pass 4g) with zero-branch fallthrough into `T`:
        //    Layout: `[b_entry, b_c1, b_t, b_exit (Ret), b_dup0 (6 ops), b_dup1 (6 ops)]`
        let mut func = new_post_ra();
        let b_entry = func.new_block(false);
        let b_c1 = func.new_block(false);
        let b_t = func.new_block(false);
        let b_exit = func.new_block(false);
        let b_dup0 = func.new_block(false);
        let b_dup1 = func.new_block(false);

        func.begin_block(b_entry);
        let c_e = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c_e, 0);
        let br_e = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_dup0 },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_e, 0);
        func.end_block(b_entry);

        func.begin_block(b_c1);
        let c_1 = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 1, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(c_1, 0);
        let br_1 = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b_dup1 },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_1, 0);
        func.push_inst(MOp::B { target: b_exit }, MInstFlags::TERMINATOR);
        func.end_block(b_c1);

        func.begin_block(b_t);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X3),
                rm: MReg::p(PReg::X4),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        for reg in [PReg::X5, PReg::X6, PReg::X7] {
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::p(PReg::X0),
                    rn: MReg::p(PReg::X0),
                    rm: MReg::p(reg),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_t);

        func.begin_block(b_exit);
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 99, wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        for dup in [b_dup0, b_dup1] {
            func.begin_block(dup);
            func.push_inst(
                MOp::Mov { dst: MReg::p(PReg::X3), src: MReg::p(PReg::X1), wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::Mov { dst: MReg::p(PReg::X4), src: MReg::p(PReg::X2), wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::MovImm { dst: MReg::p(PReg::X5), imm: 10, wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::MovImm { dst: MReg::p(PReg::X6), imm: 20, wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::MovImm { dst: MReg::p(PReg::X7), imm: 30, wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::B { target: b_t }, MInstFlags::TERMINATOR);
            func.end_block(dup);
        }

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        let stats = optimize_mfunc(&mut func, alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert!(stats.branches_folded >= 2, "6-inst duplicate merge + hoist must fold branches");
        assert_eq!(
            func.block_order.as_slice(),
            &[b_entry, b_c1, b_dup0, b_t, b_exit],
            "b_dup1 must merge into b_dup0 and b_dup0 must hoist directly before b_t"
        );
        let dup0_r = func.block_inst_range(b_dup0);
        assert_eq!(dup0_r.len(), 5, "hoisted b_dup0 must drop its trailing B b_t");
        assert!(
            !func.ops[dup0_r].iter().any(|op| matches!(*op, MOp::B { .. })),
            "hoisted b_dup0 must fall through into b_t with zero trailing branch"
        );
        assert!(
            !func.ops.iter().any(|op| op.branch_target() == Some(b_dup1)),
            "no branch may still target merged duplicate b_dup1"
        );
        let pos_enc = mencode(&func, alloc).expect("mencode merged/hoisted 6-inst trampoline");

        // 2. Distant 8-inst (7 `Mov`/`MovImm` + `B T`) post-`Ret` trampoline at `s_pos > t_pos + 8`
        //    (exceeds Pass 4d's 6-inst cap, so ONLY Pass 4g hoists it before fallthrough-free `T`):
        let mut func2 = new_post_ra();
        let d_entry = func2.new_block(false);
        let d_t = func2.new_block(false);
        let mut d_pads = [super::MBlock(0); 8];
        for slot in d_pads.iter_mut() {
            *slot = func2.new_block(false);
        }
        let d_exit = func2.new_block(false);
        let d_tramp = func2.new_block(false);

        func2.begin_block(d_entry);
        func2.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: d_tramp, wide: true },
            MInstFlags::TERMINATOR,
        );
        func2.push_inst(MOp::B { target: d_exit }, MInstFlags::TERMINATOR);
        func2.end_block(d_entry);

        func2.begin_block(d_t);
        func2.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X3),
                rm: MReg::p(PReg::X4),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        for reg in [PReg::X5, PReg::X6, PReg::X7, PReg::X8, PReg::X9] {
            func2.push_inst(
                MOp::AddReg {
                    dst: MReg::p(PReg::X0),
                    rn: MReg::p(PReg::X0),
                    rm: MReg::p(reg),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        func2.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func2.end_block(d_t);

        for (k, &pad) in (0u64..8).zip(d_pads.iter()) {
            func2.begin_block(pad);
            func2.push_inst(
                MOp::MovImm { dst: MReg::p(PReg::X0), imm: 100 + k, wide: true },
                MInstFlags::NONE,
            );
            func2.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
            func2.end_block(pad);
        }

        func2.begin_block(d_exit);
        func2.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 42, wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func2.end_block(d_exit);

        func2.begin_block(d_tramp);
        func2.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X3), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X4), src: MReg::p(PReg::X2), wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X5), imm: 1, wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X6), imm: 2, wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X7), imm: 3, wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X8), imm: 4, wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X9), imm: 5, wide: true },
            MInstFlags::NONE,
        );
        func2.push_inst(MOp::B { target: d_t }, MInstFlags::TERMINATOR);
        func2.end_block(d_tramp);

        func2.rebuild_cfg(alloc);
        assert_eq!(mverify(&func2), Ok(()));
        let stats2 = optimize_mfunc(&mut func2, alloc);
        assert_eq!(mverify(&func2), Ok(()));
        assert!(stats2.branches_folded >= 1, "Pass 4g must hoist distant 8-inst copy trampoline");
        assert_eq!(
            &func2.block_order.as_slice()[0..3],
            &[d_entry, d_tramp, d_t],
            "8-inst post-Ret copy trampoline must hoist directly before fallthrough-free d_t"
        );
        let tramp_r = func2.block_inst_range(d_tramp);
        assert_eq!(tramp_r.len(), 7, "hoisted 8-inst trampoline must drop its trailing B d_t");
        assert!(
            !func2.ops[tramp_r].iter().any(|op| matches!(*op, MOp::B { .. })),
            "hoisted 8-inst trampoline must have zero trailing B"
        );
        let dist_enc = mencode(&func2, alloc).expect("mencode hoisted 8-inst copy trampoline");

        // 3. Negative guard: an 8-inst post-`Ret` block with a non-copy `AddImm` op MUST NOT hoist.
        let mut neg = new_post_ra();
        let n_entry = neg.new_block(false);
        let n_t = neg.new_block(false);
        let n_exit = neg.new_block(false);
        let n_tramp = neg.new_block(false);

        neg.begin_block(n_entry);
        neg.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: n_tramp, wide: true },
            MInstFlags::TERMINATOR,
        );
        neg.push_inst(MOp::B { target: n_exit }, MInstFlags::TERMINATOR);
        neg.end_block(n_entry);

        neg.begin_block(n_t);
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_t);

        neg.begin_block(n_exit);
        neg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        neg.end_block(n_exit);

        neg.begin_block(n_tramp);
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X3), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X4), src: MReg::p(PReg::X2), wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X1),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X6), imm: 2, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X7), imm: 3, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X8), imm: 4, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X9), imm: 5, wide: true },
            MInstFlags::NONE,
        );
        neg.push_inst(MOp::B { target: n_t }, MInstFlags::TERMINATOR);
        neg.end_block(n_tramp);

        neg.rebuild_cfg(alloc);
        assert_eq!(mverify(&neg), Ok(()));
        let _ = optimize_mfunc(&mut neg, alloc);
        assert_eq!(mverify(&neg), Ok(()));
        assert_eq!(
            neg.block_order.as_slice(),
            &[n_entry, n_t, n_exit, n_tramp],
            "non-copy 8-inst post-Ret block must not be hoisted by Pass 4g"
        );
        let neg_tramp_r = neg.block_inst_range(n_tramp);
        assert_eq!(neg_tramp_r.len(), 8);
        assert_eq!(neg.ops[neg_tramp_r.end - 1], MOp::B { target: n_t });

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let pos_bytes = words_to_bytes(&pos_enc.words, alloc);
            let pos_exec =
                blaze_inproc::exec::Executable::new(&pos_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X7`.
            let f_pos: extern "C" fn(u64, u64, u64) -> u64 = unsafe { pos_exec.as_fn() };
            assert_eq!(f_pos(0, 5, 7), 72);
            assert_eq!(f_pos(1, 5, 7), 72);
            assert_eq!(f_pos(2, 5, 7), 99);

            let dist_bytes = words_to_bytes(&dist_enc.words, alloc);
            let dist_exec =
                blaze_inproc::exec::Executable::new(&dist_bytes, 0).expect("exec map must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0..=X9`.
            let f_dist: extern "C" fn(u64, u64, u64) -> u64 = unsafe { dist_exec.as_fn() };
            assert_eq!(f_dist(0, 10, 20), 45);
            assert_eq!(f_dist(1, 10, 20), 42);
        }
        let _ = (pos_enc, dist_enc);
    }

    #[test]
    fn mopt_v4w3_cmp_zero_ls_hi_and_sub_cbz_fusion() {
        let alloc = test_alloc();
        let mut f = new_post_ra();
        f.ret_live_gprs = 1;
        let b0 = f.new_block(false);
        let b1 = f.new_block(false);
        let b_zero = f.new_block(false);
        let b_nz = f.new_block(false);

        f.begin_block(b0);
        f.push_inst(
            MOp::SubReg {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp_i = f.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X2), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        f.set_vflag_def(cmp_i, 0);
        let br_i = f.push_inst(
            MOp::BCond { cond: MCond::Ls, target: b_zero },
            MInstFlags::TERMINATOR,
        );
        f.set_vflag_use(br_i, 0);
        f.end_block(b0);

        f.begin_block(b1);
        let cmp2_i = f.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X2), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        f.set_vflag_def(cmp2_i, 1);
        let br2_i = f.push_inst(
            MOp::BCond { cond: MCond::Hi, target: b_nz },
            MInstFlags::TERMINATOR,
        );
        f.set_vflag_use(br2_i, 1);
        f.push_inst(MOp::B { target: b_zero }, MInstFlags::TERMINATOR);
        f.end_block(b1);

        f.begin_block(b_zero);
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 100, wide: true }, MInstFlags::NONE);
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b_zero);

        f.begin_block(b_nz);
        f.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X2), wide: true },
            MInstFlags::NONE,
        );
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b_nz);

        f.rebuild_cfg(alloc);
        assert_eq!(mverify(&f), Ok(()));
        let _ = optimize_mfunc(&mut f, alloc);
        assert_eq!(mverify(&f), Ok(()));

        // `sub + cmp #0 + b.ls` in `b0` fuses all the way to `subs + b.eq` via Pass 1j -> Pass 5!
        let r0 = f.block_inst_range(b0);
        assert!(
            f.ops[r0.clone()]
                .iter()
                .any(|op| matches!(*op, MOp::SubReg { sets_flags: true, .. })),
            "expected sub + cmp #0 + b.ls to fuse into subs + b.eq; ops={:?}",
            &f.ops[r0]
        );
        assert!(
            !f.ops.iter().any(|op| matches!(*op, MOp::CmpImm { .. })),
            "expected both cmp #0 to be eliminated; ops={:?}",
            f.ops.as_slice()
        );

        let enc = mencode(&f, alloc).expect("mencode must succeed");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc.words, alloc);
            let exec = blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map");
            // SAFETY: Valid AAPCS64 leaf function touching only X0..X2.
            let func: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(func(42, 42), 100);
            assert_eq!(func(50, 42), 8);
            assert_eq!(func(0, 1), u64::MAX);
        }
        let _ = enc;
    }

    #[test]
    fn mopt_v4w3_const_csel_const_copy_backward_fold_and_uxtb_str() {
        let alloc = test_alloc();
        // 1. `mov w4, #1; cmp w0, #0; csel w3, w4, w1, ne` -> `csinc w3, w1, wzr, eq` + dead mov deleted.
        // 2. `mov x2, #1; mov x16, x2; mov x2, x1; mov x0, x16` -> `mov x2, x1; mov x0, #1`.
        let mut f = new_post_ra();
        f.ret_live_gprs = 1;
        let b0 = f.new_block(false);
        f.begin_block(b0);
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X4), imm: 1, wide: false }, MInstFlags::NONE);
        let ci = f.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 0, shift12: false, wide: false },
            MInstFlags::NONE,
        );
        f.set_vflag_def(ci, 0);
        let si = f.push_inst(
            MOp::Csel {
                dst: MReg::p(PReg::X3),
                rn: MReg::p(PReg::X4),
                rm: MReg::p(PReg::X1),
                cond: MCond::Ne,
                wide: false,
            },
            MInstFlags::NONE,
        );
        f.set_vflag_use(si, 0);
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X2), imm: 1, wide: true }, MInstFlags::NONE);
        f.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X16), src: MReg::p(PReg::X2), wide: true },
            MInstFlags::NONE,
        );
        f.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X2), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        f.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X16), wide: true },
            MInstFlags::NONE,
        );
        f.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X5),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X3),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        f.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X5), wide: true },
            MInstFlags::NONE,
        );
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b0);

        f.rebuild_cfg(alloc);
        assert_eq!(mverify(&f), Ok(()));
        let _ = optimize_mfunc(&mut f, alloc);
        assert_eq!(mverify(&f), Ok(()));

        let r0 = f.block_inst_range(b0);
        assert!(
            f.ops[r0.clone()].iter().any(|op| matches!(
                *op,
                MOp::Csinc { dst, rn, rm: MReg::XZR, cond: MCond::Eq, wide: false }
                    if dst == MReg::p(PReg::X3) && rn == MReg::p(PReg::X1)
            )),
            "expected MovImm(1)+Csel -> Csinc; ops={:?}",
            &f.ops[r0]
        );
        assert!(
            !f.ops[r0.clone()].iter().any(|op| matches!(
                *op,
                MOp::Mov { dst, .. } if dst == MReg::p(PReg::X16)
            )),
            "expected X16 const-copy chain eliminated; ops={:?}",
            &f.ops[r0]
        );
        assert!(
            f.ops[r0.clone()].iter().any(|op| matches!(
                *op,
                MOp::AddReg { dst, .. } if dst == MReg::p(PReg::X0)
            )),
            "expected backward copy fold add x5 + mov x0, x5 -> add x0; ops={:?}",
            &f.ops[r0]
        );

        // 3. `uxtb w6, w6; strb w6, [x0]; add x6, x0, #1; mov x0, x6; ret` -> `strb w6, [x0], #1; ret`!
        let mut g = new_post_ra();
        g.ret_live_gprs = 1;
        let gb0 = g.new_block(false);
        g.begin_block(gb0);
        g.push_inst(
            MOp::Ubfm {
                dst: MReg::p(PReg::X6),
                rn: MReg::p(PReg::X6),
                immr: 0,
                imms: 7,
                wide: false,
            },
            MInstFlags::NONE,
        );
        g.push_inst(
            MOp::Str {
                src: MReg::p(PReg::X6),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 0 },
                size: MemSize::B8,
            },
            MInstFlags::NONE,
        );
        g.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X6),
                rn: MReg::p(PReg::X0),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        g.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X6), wide: true },
            MInstFlags::NONE,
        );
        g.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        g.end_block(gb0);
        g.rebuild_cfg(alloc);
        assert_eq!(mverify(&g), Ok(()));
        let _ = optimize_mfunc(&mut g, alloc);
        assert_eq!(mverify(&g), Ok(()));
        let gr0 = g.block_inst_range(gb0);
        assert_eq!(
            &g.ops[gr0],
            &[
                MOp::Str {
                    src: MReg::p(PReg::X6),
                    addr: AddrMode::PostIdx { base: MReg::p(PReg::X0), off: 1 },
                    size: MemSize::B8,
                },
                MOp::Ret,
            ],
            "expected uxtb + strb + add + mov to collapse into single post-idx strb"
        );
    }

    #[test]
    fn mopt_v4w3_cross_block_mov_imm_and_post_ret_trampoline_eviction() {
        let alloc = test_alloc();
        // Part A: Cross-block known-`MovImm` elision (single-pred + diamond intersection + negative).
        let mut f = new_post_ra();
        f.ret_live_gprs = 1;
        let b0 = f.new_block(false);
        let b1 = f.new_block(false);
        let b_mid = f.new_block(false);

        f.begin_block(b0);
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 64, wide: true }, MInstFlags::NONE);
        f.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X1), target: b_mid, wide: true },
            MInstFlags::TERMINATOR,
        );
        f.end_block(b0);

        f.begin_block(b1);
        // Redundant: `x0 == 64` already guaranteed from `b0`!
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 64, wide: true }, MInstFlags::NONE);
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b1);

        f.begin_block(b_mid);
        // `x0 == 64` is redundant, `x2 == 99` was never set in `b0` and must stay!
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 64, wide: true }, MInstFlags::NONE);
        f.push_inst(MOp::MovImm { dst: MReg::p(PReg::X2), imm: 99, wide: true }, MInstFlags::NONE);
        f.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X2),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b_mid);

        f.rebuild_cfg(alloc);
        assert_eq!(mverify(&f), Ok(()));
        let _ = optimize_mfunc(&mut f, alloc);
        assert_eq!(mverify(&f), Ok(()));

        let r1 = f.block_inst_range(b1);
        assert_eq!(&f.ops[r1], &[MOp::Ret], "redundant x0=#64 in b1 must be elided");
        let rm = f.block_inst_range(b_mid);
        assert!(
            !f.ops[rm.clone()].iter().any(|op| matches!(
                *op,
                MOp::MovImm { dst, imm: 64, .. } if dst == MReg::p(PReg::X0)
            )),
            "redundant x0=#64 in b_mid must be elided; ops={:?}",
            &f.ops[rm]
        );
        assert!(
            f.ops[rm.clone()].iter().any(|op| matches!(
                *op,
                MOp::MovImm { dst, imm: 99, .. } if dst == MReg::p(PReg::X2)
            )),
            "non-redundant x2=#99 in b_mid MUST NOT be elided; ops={:?}",
            &f.ops[rm]
        );

        // Part B: Pass 4g Case 2 evicts post-Ret copy trampoline even when `T` has a fallthrough pred!
        let mut g = new_post_ra();
        g.ret_live_gprs = 1;
        let ge = g.new_block(false);
        let g_prev = g.new_block(false);
        let g_t = g.new_block(false);
        let g_other = g.new_block(false);
        let g_ret = g.new_block(false);
        let g_tramp = g.new_block(false);

        g.begin_block(ge);
        g.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: g_tramp, wide: true },
            MInstFlags::TERMINATOR,
        );
        g.push_inst(MOp::B { target: g_prev }, MInstFlags::TERMINATOR);
        g.end_block(ge);

        // `g_prev` falls through into `g_t` (no uncond terminator before `g_t`!).
        g.begin_block(g_prev);
        g.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                imm12: 5,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        g.end_block(g_prev);

        g.begin_block(g_t);
        g.push_inst(
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
        g.push_inst(MOp::B { target: g_ret }, MInstFlags::TERMINATOR);
        g.end_block(g_t);

        // Separate block between `g_t` and `g_ret` so `g_t`'s `B g_ret` stays an uncond terminator
        // providing a hot pre-`Ret` insertion boundary.
        g.begin_block(g_other);
        g.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 999, wide: true }, MInstFlags::NONE);
        g.push_inst(MOp::B { target: g_ret }, MInstFlags::TERMINATOR);
        g.end_block(g_other);

        g.begin_block(g_ret);
        g.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        g.end_block(g_ret);

        // Post-Ret copy trampoline targeting fallthrough-receiving `g_t`.
        g.begin_block(g_tramp);
        g.push_inst(MOp::MovImm { dst: MReg::p(PReg::X1), imm: 10, wide: true }, MInstFlags::NONE);
        g.push_inst(MOp::B { target: g_t }, MInstFlags::TERMINATOR);
        g.end_block(g_tramp);

        g.rebuild_cfg(alloc);
        assert_eq!(mverify(&g), Ok(()));
        let _ = optimize_mfunc(&mut g, alloc);
        assert_eq!(mverify(&g), Ok(()));

        let ret_pos = g.block_order.iter().position(|&b| b == g_ret).expect("g_ret present");
        let tramp_pos = g.block_order.iter().position(|&b| b == g_tramp).expect("g_tramp present");
        assert!(
            tramp_pos < ret_pos,
            "Pass 4g must evict post-Ret copy trampoline before g_ret; order={:?}",
            g.block_order.as_slice()
        );
        let g_enc = mencode(&g, alloc).expect("mencode g");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&g_enc.words, alloc);
            let exec = blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map");
            // SAFETY: Valid AAPCS64 leaf function touching only X0..X2.
            let gf: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(gf(0, 100, 7), 17);
            assert_eq!(gf(1, 100, 7), 112);
        }
        let _ = g_enc;
    }

    #[test]
    fn mopt_v4w4_fuses_uxt_into_addsub_and_narrow_lsr_into_wide_add() {
        let alloc = test_alloc();
        // 1. Post-RA `uxtb w6, w1; add x0, x0, x6; ret` -> `add x0, x0, w1, uxtb; ret`!
        let mut f = new_post_ra();
        f.ret_live_gprs = 1;
        let b0 = f.new_block(false);
        f.begin_block(b0);
        f.push_inst(
            MOp::Ubfm {
                dst: MReg::p(PReg::X6),
                rn: MReg::p(PReg::X1),
                immr: 0,
                imms: 7,
                wide: false,
            },
            MInstFlags::NONE,
        );
        f.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X6),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b0);
        f.rebuild_cfg(alloc);
        assert_eq!(mverify(&f), Ok(()));
        let _ = optimize_mfunc(&mut f, alloc);
        assert_eq!(mverify(&f), Ok(()));
        let r0 = f.block_inst_range(b0);
        assert_eq!(
            &f.ops[r0],
            &[
                MOp::AddExt {
                    dst: MReg::p(PReg::X0),
                    rn: MReg::p(PReg::X0),
                    rm: MReg::p(PReg::X1),
                    ext: super::MExt::Uxtb,
                    wide: true,
                    sets_flags: false,
                },
                MOp::Ret,
            ]
        );
        let enc = mencode(&f, alloc).expect("mencode AddExt");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc.words, alloc);
            let exec = blaze_inproc::exec::Executable::new(&bytes, 0).expect("exec map");
            // SAFETY: Valid AAPCS64 leaf function touching only X0..X1.
            let ff: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(ff(1000, 0x1234_5678_9abc_de05), 1005);
            assert_eq!(ff(1000, 0xffff_ffff_ffff_ffff), 1255);
        }
        let _ = enc;

        // 2. SSA: 32-bit `add w_sum, w_a, w_b` (zero-upper-32) -> `lsr w_shr, w_sum, #3` ->
        //    64-bit `add x_out, x_base, x_shr` fuses to `add x_out, x_base, x_sum, lsr #3`!
        let mut s = new_ssa();
        let v_base = s.alloc_vreg(VRegClass::Gpr64);
        let v_a = s.alloc_vreg(VRegClass::Gpr64);
        let v_b = s.alloc_vreg(VRegClass::Gpr64);
        let v_sum = s.alloc_vreg(VRegClass::Gpr64);
        let v_shr = s.alloc_vreg(VRegClass::Gpr64);
        let v_out = s.alloc_vreg(VRegClass::Gpr64);
        let sb0 = s.new_block(false);
        s.begin_block(sb0);
        s.push_inst(MOp::Mov { dst: MReg::V(v_base), src: MReg::p(PReg::X0), wide: true }, MInstFlags::NONE);
        s.push_inst(MOp::Mov { dst: MReg::V(v_a), src: MReg::p(PReg::X1), wide: true }, MInstFlags::NONE);
        s.push_inst(MOp::Mov { dst: MReg::V(v_b), src: MReg::p(PReg::X2), wide: true }, MInstFlags::NONE);
        s.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_sum),
                rn: MReg::V(v_a),
                rm: MReg::V(v_b),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        s.push_inst(
            MOp::Ubfm {
                dst: MReg::V(v_shr),
                rn: MReg::V(v_sum),
                immr: 3,
                imms: 31,
                wide: false,
            },
            MInstFlags::NONE,
        );
        s.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_out),
                rn: MReg::V(v_base),
                rm: MReg::V(v_shr),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        s.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_out), wide: true }, MInstFlags::NONE);
        s.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        s.end_block(sb0);
        s.rebuild_cfg(alloc);
        assert_eq!(mverify(&s), Ok(()));
        let _ = optimize_mfunc(&mut s, alloc);
        assert_eq!(mverify(&s), Ok(()));
        assert!(
            !s.ops.iter().any(|op| matches!(*op, MOp::Ubfm { .. })),
            "expected narrow LSR of zero-upper-32 VReg to fuse into 64-bit AddReg; ops={:?}",
            s.ops.as_slice()
        );
        assert!(
            s.ops.iter().any(|op| matches!(
                *op,
                MOp::AddReg { shift: Some(MShift::Lsr(3)), wide: true, .. }
            )),
            "expected 64-bit AddReg with Lsr(3); ops={:?}",
            s.ops.as_slice()
        );
    }

    #[test]
    fn mopt_v4w5_global_wide_mov_ssa_eliminates_cross_block_zext_copies() {
        let alloc = test_alloc();
        // `b0`: 32-bit `add w_v0, w_x, w_y` (zero-upper-32!) -> `mov w_v1, w_v0` (u32 -> u64 zext)
        //       -> branch to `b1` where `v1` is used in 64-bit `add`.
        // `peephole_known_zero_upper32` upgrades `mov w_v1, w_v0` -> `mov x_v1, x_v0`, and
        // `mopt_global_wide_mov_ssa` eliminates the cross-block copy completely!
        let mut s = new_ssa();
        let v_x = s.alloc_vreg(VRegClass::Gpr64);
        let v_y = s.alloc_vreg(VRegClass::Gpr64);
        let v_sum = s.alloc_vreg(VRegClass::Gpr64);
        let v_wide = s.alloc_vreg(VRegClass::Gpr64);
        let v_res = s.alloc_vreg(VRegClass::Gpr64);

        let b0 = s.new_block(false);
        let b1 = s.new_block(false);
        let b2 = s.new_block(false);

        s.begin_block(b0);
        s.push_inst(MOp::Mov { dst: MReg::V(v_x), src: MReg::p(PReg::X0), wide: true }, MInstFlags::NONE);
        s.push_inst(MOp::Mov { dst: MReg::V(v_y), src: MReg::p(PReg::X1), wide: true }, MInstFlags::NONE);
        s.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_sum),
                rn: MReg::V(v_x),
                rm: MReg::V(v_y),
                shift: None,
                wide: false,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        s.push_inst(MOp::Mov { dst: MReg::V(v_wide), src: MReg::V(v_sum), wide: false }, MInstFlags::NONE);
        s.push_inst(MOp::Cbz { rt: MReg::V(v_x), target: b2, wide: true }, MInstFlags::TERMINATOR);
        s.push_inst(MOp::B { target: b1 }, MInstFlags::TERMINATOR);
        s.end_block(b0);

        s.begin_block(b1);
        s.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_res),
                rn: MReg::V(v_wide),
                rm: MReg::V(v_y),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        s.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_res), wide: true }, MInstFlags::NONE);
        s.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        s.end_block(b1);

        s.begin_block(b2);
        s.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_wide), wide: true }, MInstFlags::NONE);
        s.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        s.end_block(b2);

        s.rebuild_cfg(alloc);
        assert_eq!(mverify(&s), Ok(()));
        let _ = optimize_mfunc(&mut s, alloc);
        assert_eq!(mverify(&s), Ok(()));

        // Cross-block copy `v_wide = mov v_sum` must be completely eliminated across b0, b1, b2!
        assert!(
            !s.ops.iter().any(|op| matches!(
                *op,
                MOp::Mov { dst, .. } if dst == MReg::V(v_wide)
            )),
            "expected cross-block v_wide copy eliminated; ops={:?}",
            s.ops.as_slice()
        );
        assert!(
            s.ops.iter().any(|op| matches!(
                *op,
                MOp::AddReg { dst, rn, .. } if dst == MReg::V(v_res) && rn == MReg::V(v_sum)
            )),
            "expected b1 to read v_sum directly; ops={:?}",
            s.ops.as_slice()
        );
    }
}

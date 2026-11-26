//! Typed AArch64 Machine IR Builder (`MEmitter<'a>`) over [`super::mfunc::MFunc`].
//!
//! Replaces raw `words: Vec<u32>`, `patches: Vec<BranchPatch>`, `block_word_off`,
//! `reloc_byte_offs`, `trap_byte_offs`, `extra_word_idxs`, and `volatile_word_idxs` in `isel.rs`
//! (and eliminates the legacy `u32`-disassembling `mach_a64.rs` pipeline) with a thin, zero-copy
//! typed builder:
//! - Emits typed [`MOp`] instructions directly into [`MFunc`] with exact physical registers
//!   ([`gpr`], [`gpr_sp`], [`vec`]) so `SP` vs `XZR` (hardware register 31) cannot be confused.
//! - Attaches relocations, traps, unwind rows, volatile markers, and jump-table anchors directly
//!   as side-band [`MNoteKind`] records on the returned [`MInst`] handle.
//! - Finalizes in a single [`MEmitter::finish_and_encode`] pipeline:
//!   `rebuild_cfg` -> `mverify` -> `mopt` -> `msched` -> `mopt` -> `mencode`.

use blaze_own::Alloc;
use blaze_tier::RelocKind;

use super::mfunc::{
    AddrMode, EncodedMFunc, FpAluOp, LogicalKind, MBlock, MCond, MEncodeError, MExt, MFunc,
    MFuncOpts, MInst, MInstFlags, MNoteKind, MOp, MReg, MShift, MemSize, PReg, ShiftKind, VReg,
    VRegClass, VecAluOp, mencode, mverify,
};
use super::mopt::{MOptStats, optimize_mfunc};
use super::msched::{MSchedStats, TargetModel, schedule_mfunc};
use crate::enc;

// -----------------------------------------------------------------------------
// Zero-`as` conversion helpers
// -----------------------------------------------------------------------------

#[inline]
const fn trunc_u8(v: u32) -> u8 {
    v.to_le_bytes()[0]
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

// -----------------------------------------------------------------------------
// Typed physical register converters (Rules 73, 76)
// -----------------------------------------------------------------------------

/// Converts a raw AArch64 GPR encoding (`0..=30` -> `PReg::Gpr`, `31` -> `PReg::Xzr`).
///
/// Any value `> 31` produces an architecturally invalid `PReg::Gpr(u8::MAX)` that `mverify`
/// rejects deterministically instead of silently masking bits.
#[inline]
pub const fn gpr(r: u32) -> MReg {
    if r <= 30 {
        MReg::P(PReg::Gpr(trunc_u8(r)))
    } else if r == 31 {
        MReg::XZR
    } else {
        MReg::P(PReg::Gpr(u8::MAX))
    }
}

/// Converts a raw AArch64 GPR-or-SP encoding (`0..=30` -> `PReg::Gpr`, `31` -> `PReg::Sp`).
///
/// Any value `> 31` produces an architecturally invalid `PReg::Gpr(u8::MAX)` that `mverify`
/// rejects deterministically instead of silently masking bits.
#[inline]
pub const fn gpr_sp(r: u32) -> MReg {
    if r <= 30 {
        MReg::P(PReg::Gpr(trunc_u8(r)))
    } else if r == 31 {
        MReg::SP
    } else {
        MReg::P(PReg::Gpr(u8::MAX))
    }
}

/// Converts a raw AArch64 SIMD/FP register encoding (`0..=31` -> `PReg::V`).
///
/// Any value `> 31` produces an architecturally invalid `PReg::V(u8::MAX)` that `mverify`
/// rejects deterministically instead of silently masking bits.
#[inline]
pub const fn vec(r: u32) -> MReg {
    if r <= 31 {
        MReg::P(PReg::V(trunc_u8(r)))
    } else {
        MReg::P(PReg::V(u8::MAX))
    }
}

// -----------------------------------------------------------------------------
// Aggregate emit/opt/sched statistics
// -----------------------------------------------------------------------------

/// Combined optimization and scheduling counters returned by [`MEmitter::finish_and_encode`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MEmitStats {
    pub opt_before_sched: MOptStats,
    pub sched: MSchedStats,
    pub opt_after_sched: MOptStats,
}

impl MEmitStats {
    #[inline]
    pub const fn total_peepholes(self) -> u32 {
        self.opt_before_sched
            .peepholes
            .saturating_add(self.opt_after_sched.peepholes)
    }

    #[inline]
    pub const fn total_movs_eliminated(self) -> u32 {
        self.opt_before_sched
            .movs_eliminated
            .saturating_add(self.opt_after_sched.movs_eliminated)
    }

    #[inline]
    pub const fn total_loads_forwarded(self) -> u32 {
        self.opt_before_sched
            .loads_forwarded
            .saturating_add(self.opt_after_sched.loads_forwarded)
    }

    #[inline]
    pub const fn total_pairs_formed(self) -> u32 {
        self.opt_before_sched
            .pairs_formed
            .saturating_add(self.opt_after_sched.pairs_formed)
    }

    #[inline]
    pub const fn total_post_idx_formed(self) -> u32 {
        self.opt_before_sched
            .post_idx_formed
            .saturating_add(self.opt_after_sched.post_idx_formed)
    }

    #[inline]
    pub const fn total_branches_folded(self) -> u32 {
        self.opt_before_sched
            .branches_folded
            .saturating_add(self.opt_after_sched.branches_folded)
    }

    #[inline]
    pub const fn total_trampolines_duped(self) -> u32 {
        self.opt_before_sched
            .trampolines_duped
            .saturating_add(self.opt_after_sched.trampolines_duped)
    }
}

// -----------------------------------------------------------------------------
// Typed Machine IR Emitter (`MEmitter<'a>`)
// -----------------------------------------------------------------------------

/// Block-oriented typed AArch64 emitter over [`MFunc`].
pub struct MEmitter<'a> {
    func: MFunc<'a>,
    cur_block: Option<MBlock>,
}

impl<'a> MEmitter<'a> {
    // -------------------------------------------------------------------------
    // Construction & register converters
    // -------------------------------------------------------------------------

    /// Creates a new [`MEmitter`] backed by `opts.alloc`.
    #[inline]
    pub fn new(opts: MFuncOpts<'a>) -> Self {
        Self { func: MFunc::new(opts), cur_block: None }
    }

    /// Converts a raw GPR index (`0..=30` -> `PReg::Gpr`, `31` -> `PReg::Xzr`).
    #[inline]
    pub const fn gpr(r: u32) -> MReg {
        gpr(r)
    }

    /// Converts a raw GPR-or-SP index (`0..=30` -> `PReg::Gpr`, `31` -> `PReg::Sp`).
    #[inline]
    pub const fn gpr_sp(r: u32) -> MReg {
        gpr_sp(r)
    }

    /// Converts a raw SIMD/FP index (`0..=31` -> `PReg::V`).
    #[inline]
    pub const fn vec(r: u32) -> MReg {
        vec(r)
    }

    // -------------------------------------------------------------------------
    // Block & function management
    // -------------------------------------------------------------------------

    /// Allocates a fresh basic block handle (alignment is set when [`Self::begin_block`] is called).
    #[inline]
    pub fn new_block(&mut self) -> MBlock {
        self.func.new_block(false)
    }

    /// Begins emitting instructions into `block`, closing any previously open block first,
    /// recording `align16`, and appending `block` to `block_order`.
    #[inline]
    pub fn begin_block(&mut self, block: MBlock, align16: bool) {
        if let Some(prev) = self.cur_block.take() {
            self.func.end_block(prev);
        }
        if let Some(slot) = self.func.block_align16.get_mut(to_idx(block.0)) {
            *slot = align16;
        }
        self.func.begin_block(block);
        self.cur_block = Some(block);
    }

    /// Closes the currently open basic block, if any.
    #[inline]
    pub fn end_block(&mut self) {
        if let Some(blk) = self.cur_block.take() {
            self.func.end_block(blk);
        }
    }

    /// Replaces the physical emission layout order of blocks.
    pub fn set_block_order(&mut self, order: &[MBlock]) {
        self.func.block_order.clear();
        self.func.block_order.reserve(order.len());
        for &blk in order {
            self.func.block_order.push(blk);
        }
    }

    /// Updates whether `block` requires 16-byte alignment during binary encoding.
    #[inline]
    pub fn set_block_align16(&mut self, block: MBlock, align16: bool) {
        if let Some(slot) = self.func.block_align16.get_mut(to_idx(block.0)) {
            *slot = align16;
        }
    }

    /// Updates the bitmask of physical GPRs live-out at `ret`.
    #[inline]
    pub fn set_ret_live_gprs(&mut self, mask: u32) {
        self.func.ret_live_gprs = mask;
    }

    /// Returns the currently open block, if any.
    #[inline]
    pub const fn current_block(&self) -> Option<MBlock> {
        self.cur_block
    }

    /// Number of instructions emitted so far across all blocks.
    #[inline]
    pub fn inst_count(&self) -> usize {
        self.func.ops.len()
    }

    /// Number of blocks allocated so far.
    #[inline]
    pub fn block_count(&self) -> usize {
        self.func.block_insts.len()
    }

    /// Borrows the underlying [`MFunc`].
    #[inline]
    pub fn func(&self) -> &MFunc<'a> {
        &self.func
    }

    /// Mutably borrows the underlying [`MFunc`].
    #[inline]
    pub fn func_mut(&mut self) -> &mut MFunc<'a> {
        &mut self.func
    }

    /// Closes any open block and returns the built [`MFunc`] without optimizing or encoding.
    #[inline]
    pub fn into_func(mut self) -> MFunc<'a> {
        self.end_block();
        self.func
    }

    /// Allocates a new SSA virtual register of `class` (for pre-RA SSA emission).
    #[inline]
    pub fn alloc_vreg(&mut self, class: VRegClass) -> VReg {
        self.func.alloc_vreg(class)
    }

    /// Appends an SSA [`MOp::ParallelCopy`] instruction.
    #[inline]
    pub fn push_parallel_copy(&mut self, pairs: &[(MReg, MReg)]) -> MInst {
        self.func.push_parallel_copy(pairs, MInstFlags::NONE)
    }

    /// Records the virtual flag ID defined by `inst`.
    #[inline]
    pub fn set_vflag_def(&mut self, inst: MInst, vflag: u32) {
        self.func.set_vflag_def(inst, vflag);
    }

    /// Records the virtual flag ID consumed by `inst`.
    #[inline]
    pub fn set_vflag_use(&mut self, inst: MInst, vflag: u32) {
        self.func.set_vflag_use(inst, vflag);
    }

    /// Appends an arbitrary typed [`MOp`] with explicit [`MInstFlags`].
    #[inline]
    pub fn push_inst(&mut self, op: MOp, flags: MInstFlags) -> MInst {
        self.func.push_inst(op, flags)
    }

    // -------------------------------------------------------------------------
    // Side-band metadata notes & attribute flags
    // -------------------------------------------------------------------------

    /// Unions `flags` onto `inst`'s attribute bitset.
    #[inline]
    pub fn add_flags(&mut self, inst: MInst, flags: MInstFlags) {
        if let Some(slot) = self.func.flags.get_mut(to_idx(inst.0)) {
            *slot = slot.union(flags);
        }
    }

    /// Attaches an arbitrary [`MNoteKind`] to `inst`.
    #[inline]
    pub fn attach_note(&mut self, inst: MInst, kind: MNoteKind) {
        self.func.attach_note(inst, kind);
    }

    /// Attaches a code relocation at `inst`'s exact emitted byte offset.
    #[inline]
    pub fn attach_reloc(&mut self, inst: MInst, kind: RelocKind, target: u32, addend: i64) {
        self.func
            .attach_note(inst, MNoteKind::Reloc { kind, target, addend });
    }

    /// Attaches trap-table row `trap_idx` at `inst`'s exact emitted byte offset and marks `MAY_TRAP`.
    #[inline]
    pub fn attach_trap(&mut self, inst: MInst, trap_idx: u32) {
        self.add_flags(inst, MInstFlags::MAY_TRAP);
        self.func.attach_note(inst, MNoteKind::Trap { trap_idx });
    }

    /// Attaches unwind row `unwind_idx` at `inst`'s exact emitted byte offset.
    #[inline]
    pub fn attach_unwind(&mut self, inst: MInst, unwind_idx: u32) {
        self.func.attach_note(inst, MNoteKind::Unwind { unwind_idx });
    }

    /// Marks `inst` as a volatile memory access (never elided, paired, forwarded, or reordered).
    #[inline]
    pub fn mark_volatile(&mut self, inst: MInst) {
        self.add_flags(inst, MInstFlags::VOLATILE);
        self.func.attach_note(inst, MNoteKind::Volatile);
    }

    /// Pins `inst` so neither `mopt` nor `msched` mutates, deletes, or reorders across it.
    #[inline]
    pub fn mark_pinned(&mut self, inst: MInst) {
        self.add_flags(inst, MInstFlags::PINNED);
    }

    /// Marks `inst` as cold-path code.
    #[inline]
    pub fn mark_cold(&mut self, inst: MInst) {
        self.add_flags(inst, MInstFlags::COLD);
    }

    /// Marks `inst` as potentially trapping so `msched` preserves trap ordering.
    #[inline]
    pub fn mark_may_trap(&mut self, inst: MInst) {
        self.add_flags(inst, MInstFlags::MAY_TRAP);
    }

    /// Anchors `inst` as a jump-table base address site for `table_id` and pins it.
    #[inline]
    pub fn attach_table_anchor(&mut self, inst: MInst, table_id: u32) {
        self.add_flags(inst, MInstFlags::PINNED);
        self.func
            .attach_note(inst, MNoteKind::JumpTableAnchor { table_id });
    }

    // -------------------------------------------------------------------------
    // ALU instructions
    // -------------------------------------------------------------------------

    #[inline]
    pub fn mov(&mut self, dst: MReg, src: MReg, wide: bool) -> MInst {
        self.func.push_inst(MOp::Mov { dst, src, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn mov_sp(&mut self, dst: MReg, src: MReg) -> MInst {
        self.func.push_inst(MOp::MovSp { dst, src }, MInstFlags::NONE)
    }

    #[inline]
    pub fn mov_imm(&mut self, dst: MReg, imm: u64, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::MovImm { dst, imm, wide }, MInstFlags::REMATERIALIZABLE)
    }

    #[inline]
    pub fn add_imm(&mut self, dst: MReg, rn: MReg, imm12: u16, shift12: bool, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::AddImm { dst, rn, imm12, shift12, wide, sets_flags: false },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn sub_imm(&mut self, dst: MReg, rn: MReg, imm12: u16, shift12: bool, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags: false },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn adds_imm(&mut self, dst: MReg, rn: MReg, imm12: u16, shift12: bool, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::AddImm { dst, rn, imm12, shift12, wide, sets_flags: true },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn subs_imm(&mut self, dst: MReg, rn: MReg, imm12: u16, shift12: bool, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags: true },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn add_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::AddReg { dst, rn, rm, shift, wide, sets_flags: false },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn sub_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::SubReg { dst, rn, rm, shift, wide, sets_flags: false },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn adds_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::AddReg { dst, rn, rm, shift, wide, sets_flags: true },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn subs_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::SubReg { dst, rn, rm, shift, wide, sets_flags: true },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn add_ext(
        &mut self,
        dst: MReg,
        rn: MReg,
        rm: MReg,
        ext: MExt,
        wide: bool,
        sets_flags: bool,
    ) -> MInst {
        self.func
            .push_inst(MOp::AddExt { dst, rn, rm, ext, wide, sets_flags }, MInstFlags::NONE)
    }

    #[inline]
    pub fn sub_ext(
        &mut self,
        dst: MReg,
        rn: MReg,
        rm: MReg,
        ext: MExt,
        wide: bool,
        sets_flags: bool,
    ) -> MInst {
        self.func
            .push_inst(MOp::SubExt { dst, rn, rm, ext, wide, sets_flags }, MInstFlags::NONE)
    }

    #[inline]
    pub fn and_imm(&mut self, dst: MReg, rn: MReg, imm: u64, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalImm { dst, rn, imm, kind: LogicalKind::And, wide },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn orr_imm(&mut self, dst: MReg, rn: MReg, imm: u64, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalImm { dst, rn, imm, kind: LogicalKind::Orr, wide },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn eor_imm(&mut self, dst: MReg, rn: MReg, imm: u64, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalImm { dst, rn, imm, kind: LogicalKind::Eor, wide },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn ands_imm(&mut self, dst: MReg, rn: MReg, imm: u64, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalImm { dst, rn, imm, kind: LogicalKind::Ands, wide },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn and_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: false,
                kind: LogicalKind::And,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn orr_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: false,
                kind: LogicalKind::Orr,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn eor_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: false,
                kind: LogicalKind::Eor,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn ands_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: false,
                kind: LogicalKind::Ands,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn bic_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: true,
                kind: LogicalKind::And,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn orn_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: true,
                kind: LogicalKind::Orr,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn eon_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: true,
                kind: LogicalKind::Eor,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn bics_reg(&mut self, dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func.push_inst(
            MOp::LogicalReg {
                dst,
                rn,
                rm,
                shift,
                invert_rm: true,
                kind: LogicalKind::Ands,
                wide,
            },
            MInstFlags::NONE,
        )
    }

    #[inline]
    pub fn mvn(&mut self, dst: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Mvn { dst, rm, shift, wide }, MInstFlags::NONE)
    }

    /// Emits `neg dst, rm` (`sub dst, xzr, rm`).
    #[inline]
    pub fn neg(&mut self, dst: MReg, rm: MReg, wide: bool) -> MInst {
        self.sub_reg(dst, MReg::XZR, rm, None, wide)
    }

    #[inline]
    pub fn shift_var(&mut self, dst: MReg, rn: MReg, rm: MReg, kind: ShiftKind, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::ShiftVar { dst, rn, rm, kind, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn ubfm(&mut self, dst: MReg, rn: MReg, immr: u8, imms: u8, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Ubfm { dst, rn, immr, imms, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn sbfm(&mut self, dst: MReg, rn: MReg, immr: u8, imms: u8, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Sbfm { dst, rn, immr, imms, wide }, MInstFlags::NONE)
    }

    /// Emits `lsl dst, rn, #shift` via `Ubfm` (or `Mov` when `shift == 0`).
    #[inline]
    pub fn lsl_imm(&mut self, dst: MReg, rn: MReg, shift: u8, wide: bool) -> MInst {
        if shift == 0 {
            return self.mov(dst, rn, wide);
        }
        let width: u8 = if wide { 64 } else { 32 };
        if shift >= width {
            return self.ubfm(dst, rn, width, width, wide);
        }
        self.ubfm(dst, rn, width - shift, (width - 1) - shift, wide)
    }

    /// Emits `lsr dst, rn, #shift` via `Ubfm`.
    #[inline]
    pub fn lsr_imm(&mut self, dst: MReg, rn: MReg, shift: u8, wide: bool) -> MInst {
        let imms: u8 = if wide { 63 } else { 31 };
        self.ubfm(dst, rn, shift, imms, wide)
    }

    /// Emits `asr dst, rn, #shift` via `Sbfm`.
    #[inline]
    pub fn asr_imm(&mut self, dst: MReg, rn: MReg, shift: u8, wide: bool) -> MInst {
        let imms: u8 = if wide { 63 } else { 31 };
        self.sbfm(dst, rn, shift, imms, wide)
    }

    /// Emits `ror dst, rn, #shift` via `Extr { dst, rn, rm: rn, lsb: shift, wide }`.
    #[inline]
    pub fn ror_imm(&mut self, dst: MReg, rn: MReg, shift: u8, wide: bool) -> MInst {
        self.extr(dst, rn, rn, shift, wide)
    }

    /// Emits `ubfx dst, rn, #lsb, #width` via `Ubfm`.
    #[inline]
    pub fn ubfx(&mut self, dst: MReg, rn: MReg, lsb: u8, width: u8, wide: bool) -> MInst {
        let imms = if width == 0 {
            u8::MAX
        } else {
            lsb.saturating_add(width - 1)
        };
        self.ubfm(dst, rn, lsb, imms, wide)
    }

    /// Emits `sbfx dst, rn, #lsb, #width` via `Sbfm`.
    #[inline]
    pub fn sbfx(&mut self, dst: MReg, rn: MReg, lsb: u8, width: u8, wide: bool) -> MInst {
        let imms = if width == 0 {
            u8::MAX
        } else {
            lsb.saturating_add(width - 1)
        };
        self.sbfm(dst, rn, lsb, imms, wide)
    }

    /// Zero-extends the low `from_bits` (`1..=63`) of `rn` into 64-bit `dst` (`ubfm #0, #(bits-1)`).
    #[inline]
    pub fn zext(&mut self, dst: MReg, rn: MReg, from_bits: u8) -> MInst {
        let imms = if from_bits == 0 { u8::MAX } else { from_bits - 1 };
        self.ubfm(dst, rn, 0, imms, true)
    }

    /// Sign-extends the low `from_bits` (`1..=63`) of `rn` into 64-bit `dst` (`sbfm #0, #(bits-1)`).
    #[inline]
    pub fn sext(&mut self, dst: MReg, rn: MReg, from_bits: u8) -> MInst {
        let imms = if from_bits == 0 { u8::MAX } else { from_bits - 1 };
        self.sbfm(dst, rn, 0, imms, true)
    }

    #[inline]
    pub fn bfxil(&mut self, dst: MReg, rn: MReg, lsb: u8, width: u8, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Bfxil { dst, rn, lsb, width, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn extr(&mut self, dst: MReg, rn: MReg, rm: MReg, lsb: u8, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Extr { dst, rn, rm, lsb, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn rbit(&mut self, dst: MReg, rn: MReg, wide: bool) -> MInst {
        self.func.push_inst(MOp::Rbit { dst, rn, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn clz(&mut self, dst: MReg, rn: MReg, wide: bool) -> MInst {
        self.func.push_inst(MOp::Clz { dst, rn, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn rev(&mut self, dst: MReg, rn: MReg, wide: bool) -> MInst {
        self.func.push_inst(MOp::Rev { dst, rn, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn mul(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Mul { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn madd(&mut self, dst: MReg, rn: MReg, rm: MReg, ra: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Madd { dst, rn, rm, ra, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn msub(&mut self, dst: MReg, rn: MReg, rm: MReg, ra: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Msub { dst, rn, rm, ra, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn umulh(&mut self, dst: MReg, rn: MReg, rm: MReg) -> MInst {
        self.func
            .push_inst(MOp::Umulh { dst, rn, rm }, MInstFlags::NONE)
    }

    #[inline]
    pub fn smulh(&mut self, dst: MReg, rn: MReg, rm: MReg) -> MInst {
        self.func
            .push_inst(MOp::Smulh { dst, rn, rm }, MInstFlags::NONE)
    }

    #[inline]
    pub fn udiv(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Udiv { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn sdiv(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Sdiv { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn umin(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Umin { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn umax(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Umax { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn smin(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Smin { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn smax(&mut self, dst: MReg, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Smax { dst, rn, rm, wide }, MInstFlags::NONE)
    }

    // -------------------------------------------------------------------------
    // Flags & conditional select instructions
    // -------------------------------------------------------------------------

    #[inline]
    pub fn cmp_imm(&mut self, rn: MReg, imm12: u16, shift12: bool, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::CmpImm { rn, imm12, shift12, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn cmp_reg(&mut self, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::CmpReg { rn, rm, shift, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn tst_imm(&mut self, rn: MReg, imm: u64, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::TstImm { rn, imm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn tst_reg(&mut self, rn: MReg, rm: MReg, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::TstReg { rn, rm, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn ccmp_imm(&mut self, rn: MReg, imm5: u8, nzcv: u8, cond: MCond, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::CcmpImm { rn, imm5, nzcv, cond, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn ccmp_reg(&mut self, rn: MReg, rm: MReg, nzcv: u8, cond: MCond, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::CcmpReg { rn, rm, nzcv, cond, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn csel(&mut self, dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Csel { dst, rn, rm, cond, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn csinc(&mut self, dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Csinc { dst, rn, rm, cond, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn csinv(&mut self, dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Csinv { dst, rn, rm, cond, wide }, MInstFlags::NONE)
    }

    #[inline]
    pub fn csneg(&mut self, dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Csneg { dst, rn, rm, cond, wide }, MInstFlags::NONE)
    }

    /// Emits `cset dst, cond` (`csinc dst, xzr, xzr, cond.invert()`).
    #[inline]
    pub fn cset(&mut self, dst: MReg, cond: MCond, wide: bool) -> MInst {
        self.csinc(dst, MReg::XZR, MReg::XZR, cond.invert(), wide)
    }

    /// Emits `cinc dst, rn, cond` (`csinc dst, rn, rn, cond.invert()`).
    #[inline]
    pub fn cinc(&mut self, dst: MReg, rn: MReg, cond: MCond, wide: bool) -> MInst {
        self.csinc(dst, rn, rn, cond.invert(), wide)
    }

    /// Emits `csetm dst, cond` (`csinv dst, xzr, xzr, cond.invert()`).
    #[inline]
    pub fn csetm(&mut self, dst: MReg, cond: MCond, wide: bool) -> MInst {
        self.csinv(dst, MReg::XZR, MReg::XZR, cond.invert(), wide)
    }

    // -------------------------------------------------------------------------
    // Memory instructions
    // -------------------------------------------------------------------------

    #[inline]
    pub fn ldr_off(&mut self, dst: MReg, base: MReg, off: i32, size: MemSize, signed: bool) -> MInst {
        self.func.push_inst(
            MOp::Ldr { dst, addr: AddrMode::BaseOff { base, off }, size, signed },
            MInstFlags::MEM_READ,
        )
    }

    #[inline]
    pub fn ldr_scaled(
        &mut self,
        dst: MReg,
        base: MReg,
        idx: MReg,
        shift: u8,
        size: MemSize,
        signed: bool,
    ) -> MInst {
        self.func.push_inst(
            MOp::Ldr { dst, addr: AddrMode::ScaledIdx { base, idx, shift }, size, signed },
            MInstFlags::MEM_READ,
        )
    }

    #[inline]
    pub fn ldr_post(&mut self, dst: MReg, base: MReg, off: i16, size: MemSize, signed: bool) -> MInst {
        self.func.push_inst(
            MOp::Ldr { dst, addr: AddrMode::PostIdx { base, off }, size, signed },
            MInstFlags::MEM_READ,
        )
    }

    #[inline]
    pub fn ldr_pre(&mut self, dst: MReg, base: MReg, off: i16, size: MemSize, signed: bool) -> MInst {
        self.func.push_inst(
            MOp::Ldr { dst, addr: AddrMode::PreIdx { base, off }, size, signed },
            MInstFlags::MEM_READ,
        )
    }

    #[inline]
    pub fn str_off(&mut self, src: MReg, base: MReg, off: i32, size: MemSize) -> MInst {
        self.func.push_inst(
            MOp::Str { src, addr: AddrMode::BaseOff { base, off }, size },
            MInstFlags::MEM_WRITE,
        )
    }

    #[inline]
    pub fn str_scaled(&mut self, src: MReg, base: MReg, idx: MReg, shift: u8, size: MemSize) -> MInst {
        self.func.push_inst(
            MOp::Str { src, addr: AddrMode::ScaledIdx { base, idx, shift }, size },
            MInstFlags::MEM_WRITE,
        )
    }

    #[inline]
    pub fn str_post(&mut self, src: MReg, base: MReg, off: i16, size: MemSize) -> MInst {
        self.func.push_inst(
            MOp::Str { src, addr: AddrMode::PostIdx { base, off }, size },
            MInstFlags::MEM_WRITE,
        )
    }

    #[inline]
    pub fn str_pre(&mut self, src: MReg, base: MReg, off: i16, size: MemSize) -> MInst {
        self.func.push_inst(
            MOp::Str { src, addr: AddrMode::PreIdx { base, off }, size },
            MInstFlags::MEM_WRITE,
        )
    }

    #[inline]
    pub fn ldp(
        &mut self,
        dst0: MReg,
        dst1: MReg,
        base: MReg,
        off: i16,
        size: MemSize,
        post_idx: bool,
        pre_idx: bool,
    ) -> MInst {
        self.func.push_inst(
            MOp::Ldp { dst0, dst1, base, off, size, post_idx, pre_idx },
            MInstFlags::MEM_READ,
        )
    }

    #[inline]
    pub fn stp(
        &mut self,
        src0: MReg,
        src1: MReg,
        base: MReg,
        off: i16,
        size: MemSize,
        post_idx: bool,
        pre_idx: bool,
    ) -> MInst {
        self.func.push_inst(
            MOp::Stp { src0, src1, base, off, size, post_idx, pre_idx },
            MInstFlags::MEM_WRITE,
        )
    }

    // -------------------------------------------------------------------------
    // Floating-point & NEON SIMD instructions
    // -------------------------------------------------------------------------

    #[inline]
    pub fn fp_alu(
        &mut self,
        op: FpAluOp,
        dst: MReg,
        rn: MReg,
        rm: Option<MReg>,
        ra: Option<MReg>,
        double: bool,
    ) -> MInst {
        self.func
            .push_inst(MOp::FpAlu { op, dst, rn, rm, ra, double }, MInstFlags::NONE)
    }

    #[inline]
    pub fn fp_cmp(&mut self, rn: MReg, rm: MReg, double: bool) -> MInst {
        self.func
            .push_inst(MOp::FpCmp { rn, rm, double }, MInstFlags::NONE)
    }

    #[inline]
    pub fn fcsel(&mut self, dst: MReg, rn: MReg, rm: MReg, cond: MCond, double: bool) -> MInst {
        self.func
            .push_inst(MOp::Fcsel { dst, rn, rm, cond, double }, MInstFlags::NONE)
    }

    #[inline]
    pub fn vec_alu(
        &mut self,
        op: VecAluOp,
        dst: MReg,
        rn: MReg,
        rm: Option<MReg>,
        elem_size: u8,
    ) -> MInst {
        self.func
            .push_inst(MOp::VecAlu { op, dst, rn, rm, elem_size }, MInstFlags::NONE)
    }

    // -------------------------------------------------------------------------
    // PC-relative & control-flow instructions
    // -------------------------------------------------------------------------

    #[inline]
    pub fn adrp(&mut self, dst: MReg) -> MInst {
        self.func.push_inst(MOp::Adrp { dst }, MInstFlags::NONE)
    }

    #[inline]
    pub fn add_lo12(&mut self, dst: MReg, rn: MReg) -> MInst {
        self.func.push_inst(MOp::AddLo12 { dst, rn }, MInstFlags::NONE)
    }

    #[inline]
    pub fn adr_table(&mut self, dst: MReg, table_id: u32) -> MInst {
        self.func
            .push_inst(MOp::AdrTable { dst, table_id }, MInstFlags::PINNED)
    }

    #[inline]
    pub fn bl(&mut self) -> MInst {
        self.func.push_inst(MOp::Bl, MInstFlags::CALL)
    }

    #[inline]
    pub fn blr(&mut self, target: MReg) -> MInst {
        self.func.push_inst(MOp::Blr { target }, MInstFlags::CALL)
    }

    #[inline]
    pub fn br(&mut self, target: MReg) -> MInst {
        self.func.push_inst(MOp::Br { target }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn b(&mut self, target: MBlock) -> MInst {
        self.func.push_inst(MOp::B { target }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn b_cond(&mut self, cond: MCond, target: MBlock) -> MInst {
        self.func
            .push_inst(MOp::BCond { cond, target }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn cbz(&mut self, rt: MReg, target: MBlock, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Cbz { rt, target, wide }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn cbnz(&mut self, rt: MReg, target: MBlock, wide: bool) -> MInst {
        self.func
            .push_inst(MOp::Cbnz { rt, target, wide }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn tbz(&mut self, rt: MReg, bit: u8, target: MBlock) -> MInst {
        self.func
            .push_inst(MOp::Tbz { rt, bit, target }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn tbnz(&mut self, rt: MReg, bit: u8, target: MBlock) -> MInst {
        self.func
            .push_inst(MOp::Tbnz { rt, bit, target }, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn ret(&mut self) -> MInst {
        self.func.push_inst(MOp::Ret, MInstFlags::TERMINATOR)
    }

    #[inline]
    pub fn brk(&mut self, imm: u16) -> MInst {
        self.func.push_inst(
            MOp::Brk { imm },
            MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP),
        )
    }

    #[inline]
    pub fn udf(&mut self) -> MInst {
        self.func.push_inst(
            MOp::Udf,
            MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP),
        )
    }

    #[inline]
    pub fn nop(&mut self) -> MInst {
        self.func.push_inst(MOp::Nop, MInstFlags::NONE)
    }

    // -------------------------------------------------------------------------
    // Explicit raw-word escape hatch (LSE / SME2 / CRC32 / SHA3 / sysreg)
    // -------------------------------------------------------------------------

    /// Emits a raw 32-bit AArch64 instruction word with explicit register/flag/barrier effects.
    #[inline]
    pub fn raw_word(
        &mut self,
        word: u32,
        defs_gpr: u32,
        uses_gpr: u32,
        defs_fp: u32,
        uses_fp: u32,
        writes_flags: bool,
        reads_flags: bool,
        is_mem_barrier: bool,
        pinned: bool,
    ) -> MInst {
        let mut flags = MInstFlags::NONE;
        if pinned {
            flags = flags.union(MInstFlags::PINNED);
        }
        if is_mem_barrier {
            flags = flags.union(MInstFlags::MEM_BARRIER);
        }
        if enc::is_unconditional_terminator(word) {
            flags = flags.union(MInstFlags::TERMINATOR);
        }
        self.func.push_inst(
            MOp::RawWord {
                word,
                defs_gpr,
                uses_gpr,
                defs_fp,
                uses_fp,
                writes_flags,
                reads_flags,
                is_mem_barrier,
            },
            flags,
        )
    }

    // -------------------------------------------------------------------------
    // End-to-end optimize -> schedule -> optimize -> encode pipeline
    // -------------------------------------------------------------------------

    /// Closes any open block, rebuilds the CFG, verifies invariants, runs
    /// `mopt` -> `msched` -> `mopt`, and encodes the final AArch64 words and exact side-band
    /// relocation/trap/unwind/jump-table offsets in one pass.
    pub fn finish_and_encode(
        mut self,
        target: TargetModel,
        alloc: Alloc<'a>,
    ) -> Result<(EncodedMFunc<'a>, MEmitStats), MEncodeError> {
        self.end_block();
        if self.func.is_ssa {
            return Err(MEncodeError::StillInSsa);
        }
        self.func.rebuild_cfg(alloc);
        mverify(&self.func).map_err(MEncodeError::VerifyFailed)?;

        let opt_before_sched = optimize_mfunc(&mut self.func, alloc);
        let sched = schedule_mfunc(&mut self.func, target, alloc);
        let opt_after_sched = optimize_mfunc(&mut self.func, alloc);
        let encoded = mencode(&self.func, alloc)?;

        Ok((
            encoded,
            MEmitStats { opt_before_sched, sched, opt_after_sched },
        ))
    }
}

// -----------------------------------------------------------------------------
// Unit & Apple Silicon Hardware Execution Tests (NO `Global` / NO `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    use blaze_tier::{Os, RelocKind};

    use super::{MEmitter, gpr, gpr_sp, to_idx, vec};
    use crate::enc::{self, Barrier, Size};
    use crate::sota::mfunc::{
        DEFAULT_RET_LIVE_GPRS, MCond, MFuncOpts, MReg, MemSize, PReg, RelocAt, TableAdrAt, TrapAt,
        UnwindAt,
    };
    use crate::sota::msched::TargetModel;

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn new_emitter(os: Os, alloc: Alloc<'static>) -> MEmitter<'static> {
        MEmitter::new(MFuncOpts {
            os,
            is_ssa: false,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc,
        })
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn words_to_bytes<'a>(words: &[u32], alloc: Alloc<'a>) -> blaze_own::Vec<u8, Alloc<'a>> {
        let mut bytes = blaze_own::Vec::with_capacity_in(words.len() * 4, alloc);
        for &w in words {
            for b in w.to_le_bytes() {
                bytes.push(b);
            }
        }
        bytes
    }

    #[test]
    fn memitter_builds_optimizes_schedules_and_executes_loop_on_apple_silicon() {
        // Sanity-check register converters first.
        assert_eq!(gpr(0), MReg::P(PReg::X0));
        assert_eq!(gpr(30), MReg::P(PReg::LR));
        assert_eq!(gpr(31), MReg::XZR);
        assert_eq!(gpr_sp(31), MReg::SP);
        assert_eq!(vec(7), MReg::P(PReg::V(7)));

        let alloc = test_alloc();
        let mut em = new_emitter(Os::MacOs, alloc);

        let b_entry = em.new_block();
        let b_check_len = em.new_block();
        let b_loop = em.new_block();
        let b_exit = em.new_block();
        let b_trap = em.new_block();

        // Entry: x0 = ptr (*const u64), x1 = n_pairs (u64); x2 = acc = 0.
        // Null pointer jumps to cold trap block carrying `Reloc` + `Trap` notes.
        em.begin_block(b_entry, false);
        em.mov_imm(gpr(2), 0, true);
        em.cbz(gpr(0), b_trap, true);
        em.end_block();

        // Check zero length; explicit fallthrough `b b_loop` folded by `mopt`.
        em.begin_block(b_check_len, false);
        em.cbz(gpr(1), b_exit, true);
        em.b(b_loop);
        em.end_block();

        // Hot 16B-aligned loop computing `sum_{k=0..n_pairs} (2 * ptr[2k] + ptr[2k + 1])`:
        // Exercises:
        // - Redundant load (`ldr x5, [x0, #0]` forwarded from `ldr x3, [x0, #0]`)
        // - `Ldr` + `Ldr` -> `Ldp x3, x4, [x0, #0]`
        // - `Ldp` + `AddImm x0, x0, #16` -> `PostIdx` `ldp x3, x4, [x0], #16`
        // - `SubImm x1, x1, #1` + `CmpImm x1, #0` + `b.ne` -> `subs x1, x1, #1; b.ne`
        // - `msched` loop-carried recurrence boosting on `x0`/`x1`/`x2`
        em.begin_block(b_loop, true);
        em.ldr_off(gpr(3), gpr_sp(0), 0, MemSize::X64, false);
        em.ldr_off(gpr(4), gpr_sp(0), 8, MemSize::X64, false);
        em.ldr_off(gpr(5), gpr_sp(0), 0, MemSize::X64, false);
        em.add_reg(gpr(6), gpr(3), gpr(4), None, true);
        em.add_reg(gpr(6), gpr(6), gpr(5), None, true);
        em.add_reg(gpr(2), gpr(2), gpr(6), None, true);
        em.add_imm(gpr_sp(0), gpr_sp(0), 16, false, true);
        em.sub_imm(gpr(1), gpr(1), 1, false, true);
        em.cmp_imm(gpr(1), 0, false, true);
        em.b_cond(MCond::Ne, b_loop);
        em.end_block();

        // Exit: return accumulated sum in `x0`.
        em.begin_block(b_exit, false);
        em.mov(gpr(0), gpr(2), true);
        em.ret();
        em.end_block();

        // Cold trap path with `Reloc` (`Page21` + `PageOff12`) and `Trap` notes.
        em.begin_block(b_trap, false);
        let i_adrp = em.adrp(gpr(9));
        em.attach_reloc(i_adrp, RelocKind::Page21, 7, 0);
        let i_lo12 = em.add_lo12(gpr_sp(9), gpr_sp(9));
        em.attach_reloc(i_lo12, RelocKind::PageOff12 { scale: 0 }, 7, 0);
        let i_brk = em.brk(0xff00);
        em.attach_trap(i_brk, 3);
        em.end_block();

        let (enc_out, stats) = em
            .finish_and_encode(TargetModel::APPLE_M5, alloc)
            .expect("finish_and_encode must succeed");

        // Verify all optimization + scheduling passes fired cleanly.
        assert!(
            stats.total_loads_forwarded() >= 1,
            "redundant load must be forwarded: {stats:?}"
        );
        assert!(
            stats.total_pairs_formed() >= 1,
            "adjacent loads must pair into LDP: {stats:?}"
        );
        assert!(
            stats.total_post_idx_formed() >= 1,
            "LDP + add #16 must fuse into post-index LDP: {stats:?}"
        );
        assert!(
            stats.total_peepholes() >= 1,
            "sub #1 + cmp #0 must fuse into subs #1: {stats:?}"
        );
        assert!(
            stats.total_branches_folded() >= 1,
            "fallthrough `b b_loop` must be elided: {stats:?}"
        );
        assert!(
            stats.sched.regions_scheduled >= 1,
            "scheduler must schedule loop block: {stats:?}"
        );
        assert!(
            stats.sched.recurrences_boosted >= 1,
            "scheduler must boost loop-carried recurrences: {stats:?}"
        );

        // Verify 16-byte loop alignment (`b_loop` is a backward-branch loop header).
        assert_eq!(
            enc_out.block_word_off[to_idx(b_loop.0)] % 4,
            0,
            "b_loop must be 16-byte aligned"
        );

        // Verify post-index `ldp x3, x4, [x0], #16` and fused `subs x1, x1, #1` were emitted.
        let want_ldp_post = enc::ldp_post(3, 4, 0, 16);
        let want_subs_1 = enc::subs_imm(1, 1, 1, false);
        assert!(
            enc_out.words.iter().any(|&w| w == want_ldp_post),
            "expected post-index `ldp x3, x4, [x0], #16` in encoded words"
        );
        assert!(
            enc_out.words.iter().any(|&w| w == want_subs_1),
            "expected fused `subs x1, x1, #1` in encoded words"
        );

        // Verify exact relocation & trap byte offsets point to the exact encoded instruction words!
        assert_eq!(enc_out.reloc_byte_offs.len(), 2);
        let r0 = enc_out.reloc_byte_offs[0];
        let r1 = enc_out.reloc_byte_offs[1];
        assert_eq!(
            r0,
            RelocAt { byte_off: r0.byte_off, kind: RelocKind::Page21, target: 7, addend: 0 }
        );
        assert_eq!(
            r1,
            RelocAt {
                byte_off: r1.byte_off,
                kind: RelocKind::PageOff12 { scale: 0 },
                target: 7,
                addend: 0,
            }
        );
        assert_eq!(enc_out.words[to_idx(r0.byte_off / 4)], enc::adrp(9, 0));
        assert_eq!(enc_out.words[to_idx(r1.byte_off / 4)], enc::add_imm(9, 9, 0, false));
        assert_eq!(r1.byte_off, r0.byte_off + 4);

        assert_eq!(enc_out.trap_byte_offs.len(), 1);
        let t0 = enc_out.trap_byte_offs[0];
        assert_eq!(t0, TrapAt { byte_off: t0.byte_off, trap_idx: 3 });
        assert_eq!(enc_out.words[to_idx(t0.byte_off / 4)], enc::brk(0xff00));

        // Execute on real Apple Silicon hardware and verify exact arithmetic results.
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 frameless leaf touching only caller-saved `X0..=X6`.
            let f: extern "C" fn(*const u64, u64) -> u64 = unsafe { exec.as_fn() };

            let data: [u64; 8] = [10, 20, 3, 7, 100, 250, 1, 2];
            assert_eq!(f(data.as_ptr(), 0), 0);
            assert_eq!(f(data.as_ptr(), 1), 2 * 10 + 20);
            assert_eq!(f(data.as_ptr(), 2), (2 * 10 + 20) + (2 * 3 + 7));
            assert_eq!(
                f(data.as_ptr(), 4),
                (2 * 10 + 20) + (2 * 3 + 7) + (2 * 100 + 250) + (2 * 1 + 2)
            );
        }
    }

    #[test]
    fn memitter_preserves_pinned_raw_word_and_volatile_loads() {
        let alloc = test_alloc();
        let mut em = new_emitter(Os::MacOs, alloc);

        let b0 = em.new_block();
        let dmb_ish = enc::dmb(Barrier::Ish);

        em.begin_block(b0, false);
        // Two identical volatile loads from `[x0, #0]`: neither may be forwarded or paired!
        let ld1 = em.ldr_off(gpr(1), gpr_sp(0), 0, MemSize::X64, false);
        em.mark_volatile(ld1);

        // Pinned `RawWord` barrier between the two volatile loads with attached unwind & anchor notes.
        let i_raw = em.raw_word(dmb_ish, 0, 0, 0, 0, false, false, true, true);
        em.attach_unwind(i_raw, 11);
        em.attach_table_anchor(i_raw, 4);

        let ld2 = em.ldr_off(gpr(2), gpr_sp(0), 0, MemSize::X64, false);
        em.mark_volatile(ld2);

        em.add_reg(gpr(0), gpr(1), gpr(2), None, true);
        em.ret();
        em.end_block();

        let (enc_out, stats) = em
            .finish_and_encode(TargetModel::APPLE_M5, alloc)
            .expect("finish_and_encode must preserve pinned raw words and volatile loads");

        assert_eq!(
            stats.total_loads_forwarded(),
            0,
            "volatile loads must never be forwarded: {stats:?}"
        );
        assert_eq!(
            stats.total_pairs_formed(),
            0,
            "volatile loads must never be paired: {stats:?}"
        );

        let want_ld1 = enc::ldr(Size::X, 1, 0, 0);
        let want_ld2 = enc::ldr(Size::X, 2, 0, 0);
        let want_add = enc::add(0, 1, 2);
        let want_ret = enc::ret();
        assert_eq!(
            enc_out.words.as_slice(),
            &[want_ld1, dmb_ish, want_ld2, want_add, want_ret],
            "exact instruction sequence and pinned raw barrier must be preserved verbatim"
        );

        // Verify unwind & jump-table anchor notes point to word 1 (byte offset 4 = `dmb ish`).
        assert_eq!(
            enc_out.unwind_byte_offs.as_slice(),
            &[UnwindAt { byte_off: 4, unwind_idx: 11 }]
        );
        assert_eq!(
            enc_out.table_adr_word_idxs.as_slice(),
            &[TableAdrAt { table_id: 4, word_idx: 1 }]
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let bytes = words_to_bytes(&enc_out.words, alloc);
            let exec =
                blaze_inproc::exec::Executable::new(&bytes, 0).expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 frameless leaf reading `*ptr` twice with `dmb ish`.
            let f: extern "C" fn(*const u64) -> u64 = unsafe { exec.as_fn() };
            let val: u64 = 0x1234_5678_9abc;
            assert_eq!(f(&val), val.wrapping_mul(2));
        }
    }
}

//! Fast `#![no_std]` AArch64 [`MFunc`] Pretty-Printer & Disassembly Summary (`mfmt`).
//!
//! Formats any pre-RA SSA (`is_ssa == true`) or post-RA physical (`is_ssa == false`) [`MFunc`]
//! into clean standard ARM64 assembly syntax with typed virtual registers (`vN:g64`, `vN:f64`,
//! `vN:v128`), virtual flag defs/uses (`def vf0`, `use vf0`), instruction flags (`remat`,
//! `pinned`, `volatile`, `cold`), and chained [`MNote`] annotations (`reloc(...)`, `trap(#k)`,
//! `unwind(#k)`, `table(#k)`) directly into any [`core::fmt::Write`] sink or caller-allocated
//! [`blaze_own::String`].

use core::fmt;

use blaze_own::{Alloc, String};
use blaze_tier::RelocKind;

use super::mfunc::{
    AddrMode, FpAluOp, LogicalKind, MBlock, MCond, MExt, MFunc, MInstFlags, MNoteKind, MOp, MReg,
    MShift, MemSize, NO_NOTE, NO_VFLAG, PReg, ShiftKind, VReg, VRegClass, VecAluOp,
};

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

// -----------------------------------------------------------------------------
// Public Summary & Formatting API
// -----------------------------------------------------------------------------

/// Concise structural and instruction-mix summary of an [`MFunc`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MFuncSummary {
    /// Number of basic blocks in `func.block_order`.
    pub blocks: u32,
    /// Total instructions across `func.block_order`.
    pub insts: u32,
    /// Branch instructions (`b`, `b.cc`, `cbz`, `cbnz`, `tbz`, `tbnz`, `br`).
    pub branches: u32,
    /// Call instructions (`bl`, `blr`).
    pub calls: u32,
    /// Memory load instructions (`ldr*`, `ldp`).
    pub loads: u32,
    /// Memory store instructions (`str*`, `stp`).
    pub stores: u32,
    /// Pair memory instructions (`ldp`, `stp`).
    pub pairs: u32,
    /// Stack-pointer-based loads (`ldr*`/`ldp` with base `sp`).
    pub sp_loads: u32,
    /// Stack-pointer-based stores (`str*`/`stp` with base `sp`).
    pub sp_stores: u32,
    /// Register-to-register move instructions (`mov`, `mov sp`, `fmov` reg, `mov v.16b`).
    pub movs: u32,
    /// Total allocated virtual registers (`func.vreg_class.len()`).
    pub vregs: u32,
}

/// Computes an [`MFuncSummary`] over all blocks in `func.block_order` in a single O(N) pass.
pub fn summarize_mfunc(func: &MFunc<'_>) -> MFuncSummary {
    let mut summary = MFuncSummary {
        blocks: u32_of(func.block_order.len()),
        vregs: u32_of(func.vreg_class.len()),
        ..MFuncSummary::default()
    };

    for &block in func.block_order.iter() {
        for idx in func.block_inst_range(block) {
            let Some(op) = func.ops.get(idx) else {
                continue;
            };
            summary.insts = summary.insts.saturating_add(1);
            match *op {
                MOp::B { .. }
                | MOp::BCond { .. }
                | MOp::Cbz { .. }
                | MOp::Cbnz { .. }
                | MOp::Tbz { .. }
                | MOp::Tbnz { .. }
                | MOp::Br { .. } => {
                    summary.branches = summary.branches.saturating_add(1);
                }
                MOp::Bl | MOp::Blr { .. } => {
                    summary.calls = summary.calls.saturating_add(1);
                }
                MOp::Ldr { addr, .. } => {
                    summary.loads = summary.loads.saturating_add(1);
                    if addr_base(addr) == MReg::SP {
                        summary.sp_loads = summary.sp_loads.saturating_add(1);
                    }
                }
                MOp::Str { addr, .. } => {
                    summary.stores = summary.stores.saturating_add(1);
                    if addr_base(addr) == MReg::SP {
                        summary.sp_stores = summary.sp_stores.saturating_add(1);
                    }
                }
                MOp::Ldp { base, .. } => {
                    summary.loads = summary.loads.saturating_add(1);
                    summary.pairs = summary.pairs.saturating_add(1);
                    if base == MReg::SP {
                        summary.sp_loads = summary.sp_loads.saturating_add(1);
                    }
                }
                MOp::Stp { base, .. } => {
                    summary.stores = summary.stores.saturating_add(1);
                    summary.pairs = summary.pairs.saturating_add(1);
                    if base == MReg::SP {
                        summary.sp_stores = summary.sp_stores.saturating_add(1);
                    }
                }
                MOp::Mov { .. }
                | MOp::MovSp { .. }
                | MOp::FpAlu { op: FpAluOp::Mov, .. }
                | MOp::VecAlu { op: VecAluOp::Mov, .. } => {
                    summary.movs = summary.movs.saturating_add(1);
                }
                MOp::MovImm { .. }
                | MOp::AddImm { .. }
                | MOp::SubImm { .. }
                | MOp::AddReg { .. }
                | MOp::SubReg { .. }
                | MOp::AddExt { .. }
                | MOp::SubExt { .. }
                | MOp::LogicalImm { .. }
                | MOp::LogicalReg { .. }
                | MOp::Mvn { .. }
                | MOp::ShiftVar { .. }
                | MOp::Ubfm { .. }
                | MOp::Sbfm { .. }
                | MOp::Bfxil { .. }
                | MOp::Extr { .. }
                | MOp::Rbit { .. }
                | MOp::Clz { .. }
                | MOp::Rev { .. }
                | MOp::Mul { .. }
                | MOp::Madd { .. }
                | MOp::Msub { .. }
                | MOp::Umulh { .. }
                | MOp::Smulh { .. }
                | MOp::CarryAddSub { .. }
                | MOp::Udiv { .. }
                | MOp::Sdiv { .. }
                | MOp::Umin { .. }
                | MOp::Umax { .. }
                | MOp::Smin { .. }
                | MOp::Smax { .. }
                | MOp::CmpImm { .. }
                | MOp::CmpReg { .. }
                | MOp::TstImm { .. }
                | MOp::TstReg { .. }
                | MOp::CcmpImm { .. }
                | MOp::CcmpReg { .. }
                | MOp::Csel { .. }
                | MOp::Csinc { .. }
                | MOp::Csinv { .. }
                | MOp::Csneg { .. }
                | MOp::FpAlu { .. }
                | MOp::FpCmp { .. }
                | MOp::Fcsel { .. }
                | MOp::VecAlu { .. }
                | MOp::Adrp { .. }
                | MOp::AddLo12 { .. }
                | MOp::AdrTable { .. }
                | MOp::Ret
                | MOp::Brk { .. }
                | MOp::Prfm { .. }
                | MOp::Udf
                | MOp::Nop
                | MOp::ParallelCopy { .. }
                | MOp::RawWord { .. } => {}
            }
        }
    }

    summary
}

/// Formats `func` as human-readable ARM64 machine IR into `out`.
pub fn format_mfunc<W: fmt::Write>(func: &MFunc<'_>, out: &mut W) -> fmt::Result {
    let summary = summarize_mfunc(func);
    let mode = if func.is_ssa { "ssa" } else { "post-ra" };
    writeln!(
        out,
        "; MFunc ({mode}, blocks={}, insts={}, br={}, calls={}, sp_l={}, sp_s={}, pairs={}, vregs={})",
        summary.blocks,
        summary.insts,
        summary.branches,
        summary.calls,
        summary.sp_loads,
        summary.sp_stores,
        summary.pairs,
        summary.vregs,
    )?;

    for &block in func.block_order.iter() {
        format_block_header(func, block, out)?;
        for idx in func.block_inst_range(block) {
            let Some(op) = func.ops.get(idx) else {
                continue;
            };
            write!(out, "  {idx:4}: ")?;
            format_op(func, op, out)?;
            format_annotations(func, idx, op, out)?;
            out.write_char('\n')?;
        }
    }

    Ok(())
}

/// Convenience wrapper formatting `func` into a caller-allocated [`blaze_own::String`].
pub fn format_mfunc_string<'a>(func: &MFunc<'_>, alloc: Alloc<'a>) -> String<Alloc<'a>> {
    let mut out = String::new_in(alloc);
    let est = func.ops.len().saturating_mul(48).saturating_add(128);
    out.reserve(est);
    if format_mfunc(func, &mut out).is_err() {
        return out;
    }
    out
}

// -----------------------------------------------------------------------------
// Register, Operand, Block & Annotation Formatting Helpers
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegView {
    X,
    W,
    B,
    H,
    S,
    D,
    Q,
    V,
}

#[inline]
const fn gpr_view(wide: bool) -> RegView {
    if wide { RegView::X } else { RegView::W }
}

#[inline]
const fn fp_view(double: bool) -> RegView {
    if double { RegView::D } else { RegView::S }
}

#[inline]
const fn mem_reg_view(size: MemSize, signed: bool) -> RegView {
    match size {
        MemSize::B8 | MemSize::H16 | MemSize::W32 => {
            if signed {
                RegView::X
            } else {
                RegView::W
            }
        }
        MemSize::X64 => RegView::X,
        MemSize::S32 => RegView::S,
        MemSize::D64 => RegView::D,
        MemSize::Q128 => RegView::Q,
    }
}

#[inline]
const fn addr_base(addr: AddrMode) -> MReg {
    match addr {
        AddrMode::BaseOff { base, .. }
        | AddrMode::ScaledIdx { base, .. }
        | AddrMode::PostIdx { base, .. }
        | AddrMode::PreIdx { base, .. } => base,
    }
}

#[inline]
const fn vreg_class_tag(class: VRegClass) -> &'static str {
    match class {
        VRegClass::Gpr32 => "g32",
        VRegClass::Gpr64 => "g64",
        VRegClass::Fp32 => "f32",
        VRegClass::Fp64 => "f64",
        VRegClass::Vec128 => "v128",
        VRegClass::Nzcv => "nzcv",
    }
}

#[inline]
const fn cond_str(cond: MCond) -> &'static str {
    match cond {
        MCond::Eq => "eq",
        MCond::Ne => "ne",
        MCond::Hs => "hs",
        MCond::Lo => "lo",
        MCond::Mi => "mi",
        MCond::Pl => "pl",
        MCond::Vs => "vs",
        MCond::Vc => "vc",
        MCond::Hi => "hi",
        MCond::Ls => "ls",
        MCond::Ge => "ge",
        MCond::Lt => "lt",
        MCond::Gt => "gt",
        MCond::Le => "le",
        MCond::Al => "al",
        MCond::Nv => "nv",
    }
}

#[inline]
const fn ext_str(ext: MExt) -> &'static str {
    match ext {
        MExt::Uxtb => "uxtb",
        MExt::Uxth => "uxth",
        MExt::Uxtw => "uxtw",
        MExt::Sxtw => "sxtw",
    }
}

#[inline]
const fn vec_arr_str(elem_size: u8) -> &'static str {
    match elem_size {
        1 => "8h",
        2 => "4s",
        3 => "2d",
        _ => "16b",
    }
}

#[inline]
const fn vec_elem_str(elem_size: u8) -> &'static str {
    match elem_size {
        1 => "h",
        2 => "s",
        3 => "d",
        _ => "b",
    }
}

#[inline]
const fn vec_scalar_view(elem_size: u8) -> RegView {
    match elem_size {
        0 => RegView::B,
        1 => RegView::H,
        2 => RegView::S,
        _ => RegView::D,
    }
}

fn write_reg<W: fmt::Write>(
    func: &MFunc<'_>,
    reg: MReg,
    view: RegView,
    out: &mut W,
) -> fmt::Result {
    match reg {
        MReg::V(VReg(id)) => {
            let tag = match func.vreg_class.get(to_idx(id)) {
                Some(&class) => vreg_class_tag(class),
                None => "?",
            };
            write!(out, "v{id}:{tag}")
        }
        MReg::P(preg) => match preg {
            PReg::Gpr(n) => {
                let prefix = if view == RegView::W { 'w' } else { 'x' };
                write!(out, "{prefix}{n}")
            }
            PReg::Sp => {
                if view == RegView::W {
                    out.write_str("wsp")
                } else {
                    out.write_str("sp")
                }
            }
            PReg::Xzr => {
                if view == RegView::W {
                    out.write_str("wzr")
                } else {
                    out.write_str("xzr")
                }
            }
            PReg::V(n) => {
                let prefix = match view {
                    RegView::B => 'b',
                    RegView::H => 'h',
                    RegView::S => 's',
                    RegView::D => 'd',
                    RegView::Q => 'q',
                    RegView::X | RegView::W | RegView::V => 'v',
                };
                write!(out, "{prefix}{n}")
            }
            PReg::Nzcv => out.write_str("nzcv"),
        },
    }
}

fn write_opt_reg<W: fmt::Write>(
    func: &MFunc<'_>,
    reg: Option<MReg>,
    view: RegView,
    out: &mut W,
) -> fmt::Result {
    match reg {
        Some(r) => write_reg(func, r, view, out),
        None => out.write_char('?'),
    }
}

fn write_vec_reg<W: fmt::Write>(
    func: &MFunc<'_>,
    reg: MReg,
    arr: &str,
    out: &mut W,
) -> fmt::Result {
    write_reg(func, reg, RegView::V, out)?;
    write!(out, ".{arr}")
}

fn write_opt_vec_reg<W: fmt::Write>(
    func: &MFunc<'_>,
    reg: Option<MReg>,
    arr: &str,
    out: &mut W,
) -> fmt::Result {
    match reg {
        Some(r) => write_vec_reg(func, r, arr, out),
        None => write!(out, "?.{arr}"),
    }
}

fn write_shift<W: fmt::Write>(shift: Option<MShift>, out: &mut W) -> fmt::Result {
    let Some(sh) = shift else {
        return Ok(());
    };
    match sh {
        MShift::Lsl(n) => write!(out, ", lsl #{n}"),
        MShift::Lsr(n) => write!(out, ", lsr #{n}"),
        MShift::Asr(n) => write!(out, ", asr #{n}"),
    }
}

fn write_addr<W: fmt::Write>(func: &MFunc<'_>, addr: AddrMode, out: &mut W) -> fmt::Result {
    match addr {
        AddrMode::BaseOff { base, off } => {
            out.write_char('[')?;
            write_reg(func, base, RegView::X, out)?;
            if off != 0 {
                write!(out, ", #{off}")?;
            }
            out.write_char(']')
        }
        AddrMode::ScaledIdx { base, idx, shift } => {
            out.write_char('[')?;
            write_reg(func, base, RegView::X, out)?;
            out.write_str(", ")?;
            write_reg(func, idx, RegView::X, out)?;
            if shift != 0 {
                write!(out, ", lsl #{shift}")?;
            }
            out.write_char(']')
        }
        AddrMode::PostIdx { base, off } => {
            out.write_char('[')?;
            write_reg(func, base, RegView::X, out)?;
            write!(out, "], #{off}")
        }
        AddrMode::PreIdx { base, off } => {
            out.write_char('[')?;
            write_reg(func, base, RegView::X, out)?;
            write!(out, ", #{off}]!")
        }
    }
}

fn write_pair_addr<W: fmt::Write>(
    func: &MFunc<'_>,
    base: MReg,
    off: i16,
    post_idx: bool,
    pre_idx: bool,
    out: &mut W,
) -> fmt::Result {
    out.write_char('[')?;
    write_reg(func, base, RegView::X, out)?;
    if post_idx {
        write!(out, "], #{off}")
    } else if pre_idx {
        write!(out, ", #{off}]!")
    } else if off != 0 {
        write!(out, ", #{off}]")
    } else {
        out.write_char(']')
    }
}

fn write_block_list<W: fmt::Write>(blocks: &[MBlock], out: &mut W) -> fmt::Result {
    out.write_char('[')?;
    for (i, &blk) in blocks.iter().enumerate() {
        if i != 0 {
            out.write_str(", ")?;
        }
        write!(out, "bb{}", blk.0)?;
    }
    out.write_char(']')
}

fn format_block_header<W: fmt::Write>(
    func: &MFunc<'_>,
    block: MBlock,
    out: &mut W,
) -> fmt::Result {
    let align16 = func.block_align16.get(to_idx(block.0)).copied().unwrap_or(false);
    write!(out, "bb{}: ; ", block.0)?;
    if align16 {
        out.write_str("align16 ")?;
    }
    out.write_str("preds=")?;
    write_block_list(func.block_preds(block), out)?;
    out.write_str(" succs=")?;
    write_block_list(func.block_succs(block), out)?;
    out.write_char('\n')
}

fn write_sep<W: fmt::Write>(has_any: &mut bool, out: &mut W) -> fmt::Result {
    if *has_any {
        out.write_str(", ")
    } else {
        *has_any = true;
        out.write_str(" ; ")
    }
}

fn write_reloc_kind<W: fmt::Write>(kind: RelocKind, out: &mut W) -> fmt::Result {
    match kind {
        RelocKind::Page21 => out.write_str("page21"),
        RelocKind::PageOff12 { scale } => write!(out, "pageoff12(s={scale})"),
        RelocKind::Branch26 => out.write_str("branch26"),
        RelocKind::GotPage21 => out.write_str("got_page21"),
        RelocKind::GotPageOff12 => out.write_str("got_pageoff12"),
        RelocKind::Abs64 => out.write_str("abs64"),
        RelocKind::Pc32 => out.write_str("pc32"),
        RelocKind::GotPc32 => out.write_str("got_pc32"),
    }
}

fn format_annotations<W: fmt::Write>(
    func: &MFunc<'_>,
    idx: usize,
    op: &MOp,
    out: &mut W,
) -> fmt::Result {
    let mut has_any = false;

    let vuse = func.vflag_use.get(idx).copied().unwrap_or(NO_VFLAG);
    if vuse != NO_VFLAG {
        write_sep(&mut has_any, out)?;
        write!(out, "use vf{vuse}")?;
    }

    let vdef = func.vflag_def.get(idx).copied().unwrap_or(NO_VFLAG);
    if vdef != NO_VFLAG {
        write_sep(&mut has_any, out)?;
        write!(out, "def vf{vdef}")?;
    }

    let flags = func.flags.get(idx).copied().unwrap_or(MInstFlags::NONE);
    if flags.contains(MInstFlags::REMATERIALIZABLE) {
        write_sep(&mut has_any, out)?;
        out.write_str("remat")?;
    }
    if flags.contains(MInstFlags::PINNED) {
        write_sep(&mut has_any, out)?;
        out.write_str("pinned")?;
    }
    let printed_volatile = flags.contains(MInstFlags::VOLATILE);
    if printed_volatile {
        write_sep(&mut has_any, out)?;
        out.write_str("volatile")?;
    }
    if flags.contains(MInstFlags::COLD) {
        write_sep(&mut has_any, out)?;
        out.write_str("cold")?;
    }
    let is_raw_barrier = matches!(*op, MOp::RawWord { is_mem_barrier: true, .. });
    if flags.contains(MInstFlags::MEM_BARRIER) || is_raw_barrier {
        write_sep(&mut has_any, out)?;
        out.write_str("barrier")?;
    }

    let max_notes = func.notes.len();
    let mut visited: usize = 0;
    let mut cur = func.note_idx.get(idx).copied().unwrap_or(NO_NOTE);
    while cur != NO_NOTE && visited < max_notes {
        let Some(&note) = func.notes.get(to_idx(cur)) else {
            break;
        };
        visited += 1;
        match note.kind {
            MNoteKind::Reloc { kind, target, addend } => {
                write_sep(&mut has_any, out)?;
                out.write_str("reloc(")?;
                write_reloc_kind(kind, out)?;
                write!(out, ", #{target}")?;
                if addend > 0 {
                    write!(out, "+{addend}")?;
                } else if addend < 0 {
                    write!(out, "{addend}")?;
                }
                out.write_char(')')?;
            }
            MNoteKind::Trap { trap_idx } => {
                write_sep(&mut has_any, out)?;
                write!(out, "trap(#{trap_idx})")?;
            }
            MNoteKind::Unwind { unwind_idx } => {
                write_sep(&mut has_any, out)?;
                write!(out, "unwind(#{unwind_idx})")?;
            }
            MNoteKind::Volatile => {
                if !printed_volatile {
                    write_sep(&mut has_any, out)?;
                    out.write_str("volatile")?;
                }
            }
            MNoteKind::JumpTableAnchor { table_id } => {
                write_sep(&mut has_any, out)?;
                write!(out, "table(#{table_id})")?;
            }
        }
        cur = note.next;
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Instruction Formatter (Exhaustive over all `MOp`, `FpAluOp`, `VecAluOp`)
// -----------------------------------------------------------------------------

fn format_op<W: fmt::Write>(func: &MFunc<'_>, op: &MOp, out: &mut W) -> fmt::Result {
    match *op {
        MOp::Mov { dst, src, wide } => {
            let v = gpr_view(wide);
            out.write_str("mov ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, src, v, out)
        }
        MOp::MovImm { dst, imm, wide } => {
            let v = gpr_view(wide);
            let val = if wide { imm } else { imm & 0xffff_ffff };
            out.write_str("mov ")?;
            write_reg(func, dst, v, out)?;
            if val <= 0xffff {
                write!(out, ", #{val}")
            } else {
                write!(out, ", #0x{val:x}")
            }
        }
        MOp::MovSp { dst, src } => {
            out.write_str("mov ")?;
            write_reg(func, dst, RegView::X, out)?;
            out.write_str(", ")?;
            write_reg(func, src, RegView::X, out)
        }
        MOp::AddImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            let v = gpr_view(wide);
            out.write_str(if sets_flags { "adds " } else { "add " })?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{imm12}")?;
            if shift12 {
                out.write_str(", lsl #12")?;
            }
            Ok(())
        }
        MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            let v = gpr_view(wide);
            out.write_str(if sets_flags { "subs " } else { "sub " })?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{imm12}")?;
            if shift12 {
                out.write_str(", lsl #12")?;
            }
            Ok(())
        }
        MOp::AddReg { dst, rn, rm, shift, wide, sets_flags } => {
            let v = gpr_view(wide);
            out.write_str(if sets_flags { "adds " } else { "add " })?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write_shift(shift, out)
        }
        MOp::SubReg { dst, rn, rm, shift, wide, sets_flags } => {
            let v = gpr_view(wide);
            out.write_str(if sets_flags { "subs " } else { "sub " })?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write_shift(shift, out)
        }
        MOp::AddExt { dst, rn, rm, ext, wide, sets_flags } => {
            let v = gpr_view(wide);
            out.write_str(if sets_flags { "adds " } else { "add " })?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, RegView::W, out)?;
            write!(out, ", {}", ext_str(ext))
        }
        MOp::SubExt { dst, rn, rm, ext, wide, sets_flags } => {
            let v = gpr_view(wide);
            out.write_str(if sets_flags { "subs " } else { "sub " })?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, RegView::W, out)?;
            write!(out, ", {}", ext_str(ext))
        }
        MOp::LogicalImm { dst, rn, imm, kind, wide } => {
            let v = gpr_view(wide);
            let mnem = match kind {
                LogicalKind::And => "and",
                LogicalKind::Orr => "orr",
                LogicalKind::Eor => "eor",
                LogicalKind::Ands => "ands",
            };
            write!(out, "{mnem} ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #0x{imm:x}")
        }
        MOp::LogicalReg { dst, rn, rm, shift, invert_rm, kind, wide } => {
            let v = gpr_view(wide);
            let mnem = match (kind, invert_rm) {
                (LogicalKind::And, false) => "and",
                (LogicalKind::And, true) => "bic",
                (LogicalKind::Orr, false) => "orr",
                (LogicalKind::Orr, true) => "orn",
                (LogicalKind::Eor, false) => "eor",
                (LogicalKind::Eor, true) => "eon",
                (LogicalKind::Ands, false) => "ands",
                (LogicalKind::Ands, true) => "bics",
            };
            write!(out, "{mnem} ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write_shift(shift, out)
        }
        MOp::Mvn { dst, rm, shift, wide } => {
            let v = gpr_view(wide);
            out.write_str("mvn ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write_shift(shift, out)
        }
        MOp::ShiftVar { dst, rn, rm, kind, wide } => {
            let v = gpr_view(wide);
            let mnem = match kind {
                ShiftKind::Lsl => "lsl",
                ShiftKind::Lsr => "lsr",
                ShiftKind::Asr => "asr",
                ShiftKind::Ror => "ror",
            };
            write!(out, "{mnem} ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)
        }
        MOp::Ubfm { dst, rn, immr, imms, wide } => {
            let v = gpr_view(wide);
            out.write_str("ubfm ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{immr}, #{imms}")
        }
        MOp::Sbfm { dst, rn, immr, imms, wide } => {
            let v = gpr_view(wide);
            out.write_str("sbfm ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{immr}, #{imms}")
        }
        MOp::Bfxil { dst, rn, lsb, width, wide } => {
            let v = gpr_view(wide);
            out.write_str("bfxil ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{lsb}, #{width}")
        }
        MOp::Extr { dst, rn, rm, lsb, wide } => {
            let v = gpr_view(wide);
            out.write_str("extr ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write!(out, ", #{lsb}")
        }
        MOp::Rbit { dst, rn, wide } => format_unary_gpr(func, "rbit", dst, rn, wide, out),
        MOp::Clz { dst, rn, wide } => format_unary_gpr(func, "clz", dst, rn, wide, out),
        MOp::Rev { dst, rn, wide } => format_unary_gpr(func, "rev", dst, rn, wide, out),
        MOp::Mul { dst, rn, rm, wide } => format_binary_gpr(func, "mul", dst, rn, rm, wide, out),
        MOp::Madd { dst, rn, rm, ra, wide } => {
            format_ternary_gpr(func, "madd", dst, rn, rm, ra, wide, out)
        }
        MOp::Msub { dst, rn, rm, ra, wide } => {
            format_ternary_gpr(func, "msub", dst, rn, rm, ra, wide, out)
        }
        MOp::Umulh { dst, rn, rm } => format_binary_gpr(func, "umulh", dst, rn, rm, true, out),
        MOp::Smulh { dst, rn, rm } => format_binary_gpr(func, "smulh", dst, rn, rm, true, out),
        MOp::CarryAddSub { dst, rn, rm, is_sub, sets_flags } => {
            let mnem = match (is_sub, sets_flags) {
                (false, false) => "adc",
                (false, true) => "adcs",
                (true, false) => "sbc",
                (true, true) => "sbcs",
            };
            format_binary_gpr(func, mnem, dst, rn, rm, true, out)
        }
        MOp::Udiv { dst, rn, rm, wide } => format_binary_gpr(func, "udiv", dst, rn, rm, wide, out),
        MOp::Sdiv { dst, rn, rm, wide } => format_binary_gpr(func, "sdiv", dst, rn, rm, wide, out),
        MOp::Umin { dst, rn, rm, wide } => format_binary_gpr(func, "umin", dst, rn, rm, wide, out),
        MOp::Umax { dst, rn, rm, wide } => format_binary_gpr(func, "umax", dst, rn, rm, wide, out),
        MOp::Smin { dst, rn, rm, wide } => format_binary_gpr(func, "smin", dst, rn, rm, wide, out),
        MOp::Smax { dst, rn, rm, wide } => format_binary_gpr(func, "smax", dst, rn, rm, wide, out),
        MOp::CmpImm { rn, imm12, shift12, wide } => {
            let v = gpr_view(wide);
            out.write_str("cmp ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{imm12}")?;
            if shift12 {
                out.write_str(", lsl #12")?;
            }
            Ok(())
        }
        MOp::CmpReg { rn, rm, shift, wide } => {
            let v = gpr_view(wide);
            out.write_str("cmp ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write_shift(shift, out)
        }
        MOp::TstImm { rn, imm, wide } => {
            let v = gpr_view(wide);
            out.write_str("tst ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #0x{imm:x}")
        }
        MOp::TstReg { rn, rm, wide } => {
            let v = gpr_view(wide);
            out.write_str("tst ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)
        }
        MOp::CcmpImm { rn, imm5, nzcv, cond, wide } => {
            let v = gpr_view(wide);
            out.write_str("ccmp ")?;
            write_reg(func, rn, v, out)?;
            write!(out, ", #{imm5}, #{nzcv}, {}", cond_str(cond))
        }
        MOp::CcmpReg { rn, rm, nzcv, cond, wide } => {
            let v = gpr_view(wide);
            out.write_str("ccmp ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write!(out, ", #{nzcv}, {}", cond_str(cond))
        }
        MOp::Csel { dst, rn, rm, cond, wide } => {
            format_csel_gpr(func, "csel", dst, rn, rm, cond, wide, out)
        }
        MOp::Csinc { dst, rn, rm, cond, wide } => {
            format_csel_gpr(func, "csinc", dst, rn, rm, cond, wide, out)
        }
        MOp::Csinv { dst, rn, rm, cond, wide } => {
            format_csel_gpr(func, "csinv", dst, rn, rm, cond, wide, out)
        }
        MOp::Csneg { dst, rn, rm, cond, wide } => {
            format_csel_gpr(func, "csneg", dst, rn, rm, cond, wide, out)
        }
        MOp::Ldr { dst, addr, size, signed } => {
            let mnem = match (size, signed) {
                (MemSize::B8, false) => "ldrb",
                (MemSize::B8, true) => "ldrsb",
                (MemSize::H16, false) => "ldrh",
                (MemSize::H16, true) => "ldrsh",
                (MemSize::W32, true) => "ldrsw",
                (MemSize::W32, false)
                | (
                    MemSize::X64 | MemSize::S32 | MemSize::D64 | MemSize::Q128,
                    false | true,
                ) => "ldr",
            };
            write!(out, "{mnem} ")?;
            write_reg(func, dst, mem_reg_view(size, signed), out)?;
            out.write_str(", ")?;
            write_addr(func, addr, out)
        }
        MOp::Str { src, addr, size } => {
            let mnem = match size {
                MemSize::B8 => "strb",
                MemSize::H16 => "strh",
                MemSize::W32 | MemSize::X64 | MemSize::S32 | MemSize::D64 | MemSize::Q128 => "str",
            };
            write!(out, "{mnem} ")?;
            write_reg(func, src, mem_reg_view(size, false), out)?;
            out.write_str(", ")?;
            write_addr(func, addr, out)
        }
        MOp::Ldp { dst0, dst1, base, off, size, post_idx, pre_idx } => {
            let v = mem_reg_view(size, false);
            out.write_str("ldp ")?;
            write_reg(func, dst0, v, out)?;
            out.write_str(", ")?;
            write_reg(func, dst1, v, out)?;
            out.write_str(", ")?;
            write_pair_addr(func, base, off, post_idx, pre_idx, out)
        }
        MOp::Stp { src0, src1, base, off, size, post_idx, pre_idx } => {
            let v = mem_reg_view(size, false);
            out.write_str("stp ")?;
            write_reg(func, src0, v, out)?;
            out.write_str(", ")?;
            write_reg(func, src1, v, out)?;
            out.write_str(", ")?;
            write_pair_addr(func, base, off, post_idx, pre_idx, out)
        }
        MOp::FpAlu { op: fp_op, dst, rn, rm, ra, double } => {
            format_fp_alu(func, fp_op, dst, rn, rm, ra, double, out)
        }
        MOp::FpCmp { rn, rm, double } => {
            let v = fp_view(double);
            out.write_str("fcmp ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)
        }
        MOp::Fcsel { dst, rn, rm, cond, double } => {
            let v = fp_view(double);
            out.write_str("fcsel ")?;
            write_reg(func, dst, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, v, out)?;
            out.write_str(", ")?;
            write_reg(func, rm, v, out)?;
            write!(out, ", {}", cond_str(cond))
        }
        MOp::VecAlu { op: vec_op, dst, rn, rm, elem_size } => {
            format_vec_alu(func, vec_op, dst, rn, rm, elem_size, out)
        }
        MOp::Adrp { dst } => {
            out.write_str("adrp ")?;
            write_reg(func, dst, RegView::X, out)?;
            out.write_str(", #0")
        }
        MOp::AddLo12 { dst, rn } => {
            out.write_str("add ")?;
            write_reg(func, dst, RegView::X, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, RegView::X, out)?;
            out.write_str(", #:lo12:0")
        }
        MOp::AdrTable { dst, table_id } => {
            out.write_str("adr ")?;
            write_reg(func, dst, RegView::X, out)?;
            write!(out, ", table#{table_id}")
        }
        MOp::Bl => out.write_str("bl #0"),
        MOp::Blr { target } => {
            out.write_str("blr ")?;
            write_reg(func, target, RegView::X, out)
        }
        MOp::Br { target } => {
            out.write_str("br ")?;
            write_reg(func, target, RegView::X, out)
        }
        MOp::B { target } => write!(out, "b bb{}", target.0),
        MOp::BCond { cond, target } => write!(out, "b.{} bb{}", cond_str(cond), target.0),
        MOp::Cbz { rt, target, wide } => {
            out.write_str("cbz ")?;
            write_reg(func, rt, gpr_view(wide), out)?;
            write!(out, ", bb{}", target.0)
        }
        MOp::Cbnz { rt, target, wide } => {
            out.write_str("cbnz ")?;
            write_reg(func, rt, gpr_view(wide), out)?;
            write!(out, ", bb{}", target.0)
        }
        MOp::Tbz { rt, bit, target } => {
            out.write_str("tbz ")?;
            write_reg(func, rt, RegView::X, out)?;
            write!(out, ", #{bit}, bb{}", target.0)
        }
        MOp::Tbnz { rt, bit, target } => {
            out.write_str("tbnz ")?;
            write_reg(func, rt, RegView::X, out)?;
            write!(out, ", #{bit}, bb{}", target.0)
        }
        MOp::Ret => out.write_str("ret"),
        MOp::Brk { imm } => write!(out, "brk #0x{imm:x}"),
        MOp::Prfm { base, imm12 } => {
            out.write_str("prfm pldl1keep, [")?;
            write_reg(func, base, RegView::X, out)?;
            if imm12 != 0 {
                let bytes = u32::from(imm12).saturating_mul(8);
                write!(out, ", #0x{bytes:x}")?;
            }
            out.write_str("]")
        }
        MOp::Udf => out.write_str("udf #0"),
        MOp::Nop => out.write_str("nop"),
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len));
            match func.copy_pairs.get(s..e) {
                Some(pairs) => {
                    out.write_str("pcopy [")?;
                    for (i, &(dst, src)) in pairs.iter().enumerate() {
                        if i != 0 {
                            out.write_str(", ")?;
                        }
                        write_reg(func, dst, RegView::X, out)?;
                        out.write_str(" <- ")?;
                        write_reg(func, src, RegView::X, out)?;
                    }
                    out.write_char(']')
                }
                None => write!(out, "pcopy <invalid {start}..+{len}>"),
            }
        }
        MOp::RawWord { word, .. } => write!(out, ".word 0x{word:08x}"),
    }
}

fn format_unary_gpr<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    wide: bool,
    out: &mut W,
) -> fmt::Result {
    let v = gpr_view(wide);
    write!(out, "{mnem} ")?;
    write_reg(func, dst, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rn, v, out)
}

fn format_binary_gpr<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    rm: MReg,
    wide: bool,
    out: &mut W,
) -> fmt::Result {
    let v = gpr_view(wide);
    write!(out, "{mnem} ")?;
    write_reg(func, dst, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rn, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rm, v, out)
}

fn format_ternary_gpr<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    rm: MReg,
    ra: MReg,
    wide: bool,
    out: &mut W,
) -> fmt::Result {
    let v = gpr_view(wide);
    write!(out, "{mnem} ")?;
    write_reg(func, dst, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rn, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rm, v, out)?;
    out.write_str(", ")?;
    write_reg(func, ra, v, out)
}

fn format_csel_gpr<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    rm: MReg,
    cond: MCond,
    wide: bool,
    out: &mut W,
) -> fmt::Result {
    let v = gpr_view(wide);
    write!(out, "{mnem} ")?;
    write_reg(func, dst, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rn, v, out)?;
    out.write_str(", ")?;
    write_reg(func, rm, v, out)?;
    write!(out, ", {}", cond_str(cond))
}

fn format_fp_bin<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    fv: RegView,
    out: &mut W,
) -> fmt::Result {
    write!(out, "{mnem} ")?;
    write_reg(func, dst, fv, out)?;
    out.write_str(", ")?;
    write_reg(func, rn, fv, out)?;
    out.write_str(", ")?;
    write_opt_reg(func, rm, fv, out)
}

fn format_fp_un<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    fv: RegView,
    out: &mut W,
) -> fmt::Result {
    write!(out, "{mnem} ")?;
    write_reg(func, dst, fv, out)?;
    out.write_str(", ")?;
    write_reg(func, rn, fv, out)
}

fn format_fp_alu<W: fmt::Write>(
    func: &MFunc<'_>,
    fp_op: FpAluOp,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    ra: Option<MReg>,
    double: bool,
    out: &mut W,
) -> fmt::Result {
    let fv = fp_view(double);
    match fp_op {
        FpAluOp::Add => format_fp_bin(func, "fadd", dst, rn, rm, fv, out),
        FpAluOp::Sub => format_fp_bin(func, "fsub", dst, rn, rm, fv, out),
        FpAluOp::Mul => format_fp_bin(func, "fmul", dst, rn, rm, fv, out),
        FpAluOp::Div => format_fp_bin(func, "fdiv", dst, rn, rm, fv, out),
        FpAluOp::Madd | FpAluOp::Msub => {
            let mnem = if fp_op == FpAluOp::Madd { "fmadd" } else { "fmsub" };
            write!(out, "{mnem} ")?;
            write_reg(func, dst, fv, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, fv, out)?;
            out.write_str(", ")?;
            write_opt_reg(func, rm, fv, out)?;
            out.write_str(", ")?;
            write_opt_reg(func, ra, fv, out)
        }
        FpAluOp::Mov => format_fp_un(func, "fmov", dst, rn, fv, out),
        FpAluOp::Neg => format_fp_un(func, "fneg", dst, rn, fv, out),
        FpAluOp::Abs => format_fp_un(func, "fabs", dst, rn, fv, out),
        FpAluOp::Sqrt => format_fp_un(func, "fsqrt", dst, rn, fv, out),
        FpAluOp::RintN => format_fp_un(func, "frintn", dst, rn, fv, out),
        FpAluOp::RintP => format_fp_un(func, "frintp", dst, rn, fv, out),
        FpAluOp::RintM => format_fp_un(func, "frintm", dst, rn, fv, out),
        FpAluOp::RintZ => format_fp_un(func, "frintz", dst, rn, fv, out),
        FpAluOp::RintA => format_fp_un(func, "frinta", dst, rn, fv, out),
        FpAluOp::CvtS2D => {
            out.write_str("fcvt ")?;
            write_reg(func, dst, RegView::D, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, RegView::S, out)
        }
        FpAluOp::CvtD2S => {
            out.write_str("fcvt ")?;
            write_reg(func, dst, RegView::S, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, RegView::D, out)
        }
        FpAluOp::MovFromGpr => {
            out.write_str("fmov ")?;
            write_reg(func, dst, fv, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, gpr_view(double), out)
        }
        FpAluOp::MovToGpr => {
            out.write_str("fmov ")?;
            write_reg(func, dst, gpr_view(double), out)?;
            out.write_str(", ")?;
            write_reg(func, rn, fv, out)
        }
        FpAluOp::Scvtf { int_wide } => {
            out.write_str("scvtf ")?;
            write_reg(func, dst, fv, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, gpr_view(int_wide), out)
        }
        FpAluOp::Ucvtf { int_wide } => {
            out.write_str("ucvtf ")?;
            write_reg(func, dst, fv, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, gpr_view(int_wide), out)
        }
        FpAluOp::Fcvtzs { int_wide } => {
            out.write_str("fcvtzs ")?;
            write_reg(func, dst, gpr_view(int_wide), out)?;
            out.write_str(", ")?;
            write_reg(func, rn, fv, out)
        }
        FpAluOp::Fcvtzu { int_wide } => {
            out.write_str("fcvtzu ")?;
            write_reg(func, dst, gpr_view(int_wide), out)?;
            out.write_str(", ")?;
            write_reg(func, rn, fv, out)
        }
    }
}

fn format_vec_alu<W: fmt::Write>(
    func: &MFunc<'_>,
    vec_op: VecAluOp,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    elem_size: u8,
    out: &mut W,
) -> fmt::Result {
    let arr = vec_arr_str(elem_size);
    let farr = if elem_size == 3 { "2d" } else { "4s" };
    match vec_op {
        VecAluOp::Zero => {
            out.write_str("movi ")?;
            write_vec_reg(func, dst, "16b", out)?;
            out.write_str(", #0")
        }
        VecAluOp::Mov => {
            out.write_str("mov ")?;
            write_vec_reg(func, dst, "16b", out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, "16b", out)
        }
        VecAluOp::Not => {
            out.write_str("not ")?;
            write_vec_reg(func, dst, "16b", out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, "16b", out)
        }
        VecAluOp::And => format_vec_bin(func, "and", dst, rn, rm, "16b", out),
        VecAluOp::Orr => format_vec_bin(func, "orr", dst, rn, rm, "16b", out),
        VecAluOp::Eor => format_vec_bin(func, "eor", dst, rn, rm, "16b", out),
        VecAluOp::Bsl => format_vec_bin(func, "bsl", dst, rn, rm, "16b", out),
        VecAluOp::Bit => format_vec_bin(func, "bit", dst, rn, rm, "16b", out),
        VecAluOp::Bif => format_vec_bin(func, "bif", dst, rn, rm, "16b", out),
        VecAluOp::Add => format_vec_bin(func, "add", dst, rn, rm, arr, out),
        VecAluOp::Sub => format_vec_bin(func, "sub", dst, rn, rm, arr, out),
        VecAluOp::Mul => format_vec_bin(func, "mul", dst, rn, rm, arr, out),
        VecAluOp::Mla => format_vec_bin(func, "mla", dst, rn, rm, arr, out),
        VecAluOp::Mls => format_vec_bin(func, "mls", dst, rn, rm, arr, out),
        VecAluOp::Smin => format_vec_bin(func, "smin", dst, rn, rm, arr, out),
        VecAluOp::Smax => format_vec_bin(func, "smax", dst, rn, rm, arr, out),
        VecAluOp::Umin => format_vec_bin(func, "umin", dst, rn, rm, arr, out),
        VecAluOp::Umax => format_vec_bin(func, "umax", dst, rn, rm, arr, out),
        VecAluOp::Sqadd => format_vec_bin(func, "sqadd", dst, rn, rm, arr, out),
        VecAluOp::Uqadd => format_vec_bin(func, "uqadd", dst, rn, rm, arr, out),
        VecAluOp::Sqsub => format_vec_bin(func, "sqsub", dst, rn, rm, arr, out),
        VecAluOp::Uqsub => format_vec_bin(func, "uqsub", dst, rn, rm, arr, out),
        VecAluOp::Fadd => format_vec_bin(func, "fadd", dst, rn, rm, farr, out),
        VecAluOp::Fsub => format_vec_bin(func, "fsub", dst, rn, rm, farr, out),
        VecAluOp::Fmul => format_vec_bin(func, "fmul", dst, rn, rm, farr, out),
        VecAluOp::Fmla => format_vec_bin(func, "fmla", dst, rn, rm, farr, out),
        VecAluOp::Fmls => format_vec_bin(func, "fmls", dst, rn, rm, farr, out),
        VecAluOp::Cmeq => format_vec_bin(func, "cmeq", dst, rn, rm, arr, out),
        VecAluOp::Cmgt => format_vec_bin(func, "cmgt", dst, rn, rm, arr, out),
        VecAluOp::Cmge => format_vec_bin(func, "cmge", dst, rn, rm, arr, out),
        VecAluOp::Cmhi => format_vec_bin(func, "cmhi", dst, rn, rm, arr, out),
        VecAluOp::Cmhs => format_vec_bin(func, "cmhs", dst, rn, rm, arr, out),
        VecAluOp::DupGen => {
            out.write_str("dup ")?;
            write_vec_reg(func, dst, arr, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, gpr_view(elem_size == 3), out)
        }
        VecAluOp::DupElem0 => {
            out.write_str("dup ")?;
            write_vec_reg(func, dst, arr, out)?;
            out.write_str(", ")?;
            write_reg(func, rn, RegView::V, out)?;
            write!(out, ".{}[0]", vec_elem_str(elem_size))
        }
        VecAluOp::Addv => {
            out.write_str("addv ")?;
            write_reg(func, dst, vec_scalar_view(elem_size), out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, arr, out)
        }
        VecAluOp::AddpD => {
            out.write_str("addp ")?;
            write_reg(func, dst, RegView::D, out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, "2d", out)
        }
        VecAluOp::FaddpScalar => {
            let double = elem_size == 3;
            out.write_str("faddp ")?;
            write_reg(func, dst, fp_view(double), out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, if double { "2d" } else { "2s" }, out)
        }
        VecAluOp::FaddpV => format_vec_bin(func, "faddp", dst, rn, rm, farr, out),
        VecAluOp::UminvB => {
            out.write_str("uminv ")?;
            write_reg(func, dst, RegView::B, out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, "16b", out)
        }
        VecAluOp::UmovLane { lane } => {
            out.write_str("umov ")?;
            write_reg(func, dst, gpr_view(elem_size == 3), out)?;
            out.write_str(", ")?;
            write_reg(func, rn, RegView::V, out)?;
            write!(out, ".{}[{lane}]", vec_elem_str(elem_size))
        }
        VecAluOp::Ext { imm4 } => {
            out.write_str("ext ")?;
            write_vec_reg(func, dst, "16b", out)?;
            out.write_str(", ")?;
            write_vec_reg(func, rn, "16b", out)?;
            out.write_str(", ")?;
            write_opt_vec_reg(func, rm, "16b", out)?;
            write!(out, ", #{imm4}")
        }
    }
}

fn format_vec_bin<W: fmt::Write>(
    func: &MFunc<'_>,
    mnem: &str,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    arr: &str,
    out: &mut W,
) -> fmt::Result {
    write!(out, "{mnem} ")?;
    write_vec_reg(func, dst, arr, out)?;
    out.write_str(", ")?;
    write_vec_reg(func, rn, arr, out)?;
    out.write_str(", ")?;
    write_opt_vec_reg(func, rm, arr, out)
}

// -----------------------------------------------------------------------------
// Unit Tests (explicit `blaze_own::testing::heap().alloc()`, NO `Global` / `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    use blaze_tier::{Os, RelocKind};

    use super::{MFuncSummary, format_mfunc_string, summarize_mfunc};
    use crate::sota::mfunc::{
        AddrMode, DEFAULT_RET_LIVE_GPRS, FpAluOp, MCond, MFunc, MFuncOpts, MInstFlags, MNoteKind,
        MOp, MReg, MemSize, PReg, VRegClass, mverify,
    };

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    #[test]
    fn format_ssa_mfunc_with_vregs_vflags_pcopy_and_reloc() {
        let alloc = test_alloc();
        let mut func = MFunc::new(MFuncOpts {
            os: Os::MacOs,
            is_ssa: true,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc,
        });

        let v0 = func.alloc_vreg(VRegClass::Gpr64);
        let v1 = func.alloc_vreg(VRegClass::Gpr64);
        let v2 = func.alloc_vreg(VRegClass::Gpr64);
        let vf3 = func.alloc_vreg(VRegClass::Fp64);
        let b0 = func.new_block(false);
        let b1 = func.new_block(true);

        func.begin_block(b0);
        // 0: rematerializable constant into v0:g64
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 10, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        // 1: fmov v3:f64, v0:g64
        func.push_inst(
            MOp::FpAlu {
                op: FpAluOp::MovFromGpr,
                dst: MReg::V(vf3),
                rn: MReg::V(v0),
                rm: None,
                ra: None,
                double: true,
            },
            MInstFlags::NONE,
        );
        // 2: adrp v1:g64 + pinned + reloc(page21, #7)
        let i_adrp = func.push_inst(MOp::Adrp { dst: MReg::V(v1) }, MInstFlags::PINNED);
        func.attach_note(
            i_adrp,
            MNoteKind::Reloc { kind: RelocKind::Page21, target: 7, addend: 0 },
        );
        // 3: cmp v0:g64, #5 ; def vf0
        let i_cmp = func.push_inst(
            MOp::CmpImm { rn: MReg::V(v0), imm12: 5, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(i_cmp, 0);
        // 4: csel v2:g64, v0:g64, xzr, eq ; use vf0
        let i_csel = func.push_inst(
            MOp::Csel {
                dst: MReg::V(v2),
                rn: MReg::V(v0),
                rm: MReg::XZR,
                cond: MCond::Eq,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.set_vflag_use(i_csel, 0);
        // 5: parallel copy from VRegs into physical ABI regs
        func.push_parallel_copy(
            &[(MReg::p(PReg::X0), MReg::V(v2)), (MReg::vec(0), MReg::V(vf3))],
            MInstFlags::NONE,
        );
        // 6: bl #0 with chained Reloc + Unwind notes
        let i_bl = func.push_inst(MOp::Bl, MInstFlags::CALL);
        func.attach_note(
            i_bl,
            MNoteKind::Reloc { kind: RelocKind::Branch26, target: 11, addend: 0 },
        );
        func.attach_note(i_bl, MNoteKind::Unwind { unwind_idx: 3 });
        // 7: b bb1
        func.push_inst(MOp::B { target: b1 }, MInstFlags::TERMINATOR);
        func.end_block(b0);

        func.begin_block(b1);
        // 8: ret
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b1);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let summary = summarize_mfunc(&func);
        assert_eq!(
            summary,
            MFuncSummary {
                blocks: 2,
                insts: 9,
                branches: 1,
                calls: 1,
                loads: 0,
                stores: 0,
                pairs: 0,
                sp_loads: 0,
                sp_stores: 0,
                movs: 0,
                vregs: 4,
            }
        );

        let text = format_mfunc_string(&func, alloc);
        let expected = concat!(
            "; MFunc (ssa, blocks=2, insts=9, br=1, calls=1, sp_l=0, sp_s=0, pairs=0, vregs=4)\n",
            "bb0: ; preds=[] succs=[bb1]\n",
            "     0: mov v0:g64, #10 ; remat\n",
            "     1: fmov v3:f64, v0:g64\n",
            "     2: adrp v1:g64, #0 ; pinned, reloc(page21, #7)\n",
            "     3: cmp v0:g64, #5 ; def vf0\n",
            "     4: csel v2:g64, v0:g64, xzr, eq ; use vf0\n",
            "     5: pcopy [x0 <- v2:g64, v0 <- v3:f64]\n",
            "     6: bl #0 ; unwind(#3), reloc(branch26, #11)\n",
            "     7: b bb1\n",
            "bb1: ; align16 preds=[bb0] succs=[]\n",
            "     8: ret\n",
        );
        assert_eq!(text.as_str(), expected);
    }

    #[test]
    fn format_post_ra_mfunc_with_sp_ldp_postidx_and_bcond() {
        let alloc = test_alloc();
        let mut func = MFunc::new(MFuncOpts {
            os: Os::MacOs,
            is_ssa: false,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc,
        });

        let b0 = func.new_block(false);
        let b1 = func.new_block(true);
        let b2 = func.new_block(false);

        func.begin_block(b0);
        // 0: stp x29, x30, [sp, #-16]!
        func.push_inst(
            MOp::Stp {
                src0: MReg::p(PReg::FP),
                src1: MReg::p(PReg::LR),
                base: MReg::SP,
                off: -16,
                size: MemSize::X64,
                post_idx: false,
                pre_idx: true,
            },
            MInstFlags::MEM_WRITE,
        );
        // 1: mov x29, sp
        func.push_inst(
            MOp::MovSp { dst: MReg::p(PReg::FP), src: MReg::SP },
            MInstFlags::NONE,
        );
        // 2: ldr x2, [x0], #8 ; volatile
        func.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X2),
                addr: AddrMode::PostIdx { base: MReg::p(PReg::X0), off: 8 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ.union(MInstFlags::VOLATILE),
        );
        // 3: ldr q0, [x0, #16]
        func.push_inst(
            MOp::Ldr {
                dst: MReg::vec(0),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X0), off: 16 },
                size: MemSize::Q128,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        // 4: cmp w2, #0 ; def vf0
        let i_cmp = func.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X2), imm12: 0, shift12: false, wide: false },
            MInstFlags::NONE,
        );
        func.set_vflag_def(i_cmp, 0);
        // 5: b.eq bb2 ; use vf0
        let i_br = func.push_inst(
            MOp::BCond { cond: MCond::Eq, target: b2 },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(i_br, 0);
        func.end_block(b0);

        func.begin_block(b1);
        // 6: ldp x29, x30, [sp], #16
        func.push_inst(
            MOp::Ldp {
                dst0: MReg::p(PReg::FP),
                dst1: MReg::p(PReg::LR),
                base: MReg::SP,
                off: 16,
                size: MemSize::X64,
                post_idx: true,
                pre_idx: false,
            },
            MInstFlags::MEM_READ,
        );
        // 7: adr x9, table#4 ; pinned, table(#4)
        let i_tbl = func.push_inst(
            MOp::AdrTable { dst: MReg::p(PReg::X9), table_id: 4 },
            MInstFlags::PINNED,
        );
        func.attach_note(i_tbl, MNoteKind::JumpTableAnchor { table_id: 4 });
        // 8: ret
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b1);

        func.begin_block(b2);
        // 9: brk #0xff00 ; cold, trap(#2)
        let i_brk = func.push_inst(
            MOp::Brk { imm: 0xff00 },
            MInstFlags::TERMINATOR.union(MInstFlags::COLD),
        );
        func.attach_note(i_brk, MNoteKind::Trap { trap_idx: 2 });
        func.end_block(b2);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));

        let summary = summarize_mfunc(&func);
        assert_eq!(
            summary,
            MFuncSummary {
                blocks: 3,
                insts: 10,
                branches: 1,
                calls: 0,
                loads: 3,
                stores: 1,
                pairs: 2,
                sp_loads: 1,
                sp_stores: 1,
                movs: 1,
                vregs: 0,
            }
        );

        let text = format_mfunc_string(&func, alloc);
        let expected = concat!(
            "; MFunc (post-ra, blocks=3, insts=10, br=1, calls=0, sp_l=1, sp_s=1, pairs=2, vregs=0)\n",
            "bb0: ; preds=[] succs=[bb2, bb1]\n",
            "     0: stp x29, x30, [sp, #-16]!\n",
            "     1: mov x29, sp\n",
            "     2: ldr x2, [x0], #8 ; volatile\n",
            "     3: ldr q0, [x0, #16]\n",
            "     4: cmp w2, #0 ; def vf0\n",
            "     5: b.eq bb2 ; use vf0\n",
            "bb1: ; align16 preds=[bb0] succs=[]\n",
            "     6: ldp x29, x30, [sp], #16\n",
            "     7: adr x9, table#4 ; pinned, table(#4)\n",
            "     8: ret\n",
            "bb2: ; preds=[bb0] succs=[]\n",
            "     9: brk #0xff00 ; cold, trap(#2)\n",
        );
        assert_eq!(text.as_str(), expected);
    }
}

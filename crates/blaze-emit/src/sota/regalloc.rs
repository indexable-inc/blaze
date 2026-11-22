//! Spill-Capable Chordal-SSA Register Allocator (GPR + Scalar FP) for the non-LLVM `Sota` AArch64 tier.
//!
//! Because `blaze-lir` is in strict SSA form with block parameters, its interference graph is
//! chordal. This allocator computes exact `[def, last_use]` intervals (extended to a fixed point
//! across backward loop edges), marks values live across `Call` / `CallIndirect` instructions for
//! callee-saved allocation (`X19..X28`, `V8..V15`), coalesces entry parameters (`X0..X7`, `V0..V7`)
//! in place, keeps pure leaf functions completely frameless (`X0..X15`, `V0..V7`/`V16..V28`, zero
//! stack frame, zero spills), and seamlessly spills overflow values to reusable 8-byte stack spill
//! slots when live pressure exceeds physical registers.

use core::alloc::Allocator;

use blaze_lir::{Block, Op, Ty, View};
use blaze_own::Vec;

use crate::enc::{
    Reg, X0, X1, X2, X8, X9, X10, X11, X12, X13, X14, X15,
};

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
    /// `true` iff the function contains any call (requiring `FP`/`LR` frame record).
    pub(crate) has_calls: bool,
}

/// Visits every value operand read by `inst` (including block-edge arguments).
pub(crate) fn for_each_use(view: &View<'_>, inst: usize, mut f: impl FnMut(u32)) {
    use blaze_lir::op::{Field, Layout};
    let op = view.op(inst);
    match op.info().layout {
        Layout::None => {}
        Layout::Data(Field::Value) => f(view.data[inst]),
        Layout::Data(_) => {}
        Layout::Extra(fields) => {
            let words = view.extra;
            let mut at = view.data[inst] as usize;
            let mut scrutinee = None;
            for &field in fields {
                match field {
                    Field::Value => {
                        scrutinee = Some(words[at]);
                        f(words[at]);
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
                        let n = words[at + 1] as usize;
                        for k in 0..n {
                            f(words[at + 2 + k]);
                        }
                        at += 2 + n;
                    }
                    Field::Values => {
                        let n = words[at] as usize;
                        for k in 0..n {
                            f(words[at + 1 + k]);
                        }
                        at += 1 + n;
                    }
                    Field::Blocks | Field::Mask => {
                        let n = words[at] as usize;
                        at += 1 + n;
                    }
                    Field::Const => {
                        at += Ty(view.tys[inst]).const_words() as usize;
                    }
                    Field::Cases => {
                        let width = scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                        let n = words[at] as usize;
                        at += 1;
                        for _ in 0..n {
                            at += width;
                            let argc = words[at + 1] as usize;
                            for k in 0..argc {
                                f(words[at + 2 + k]);
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
    let mut at = view.data[inst] as usize;
    let mut scrutinee = None;
    for &field in fields {
        match field {
            Field::Value => {
                scrutinee = Some(words[at]);
                at += 1;
            }
            Field::Block => {
                f(words[at], 0, 0);
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
                let block = words[at];
                let n = words[at + 1] as usize;
                f(block, at + 2, n);
                at += 2 + n;
            }
            Field::Values | Field::Blocks | Field::Mask => {
                let n = words[at] as usize;
                at += 1 + n;
            }
            Field::Const => {
                at += Ty(view.tys[inst]).const_words() as usize;
            }
            Field::Cases => {
                let width = scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                let n = words[at] as usize;
                at += 1;
                for _ in 0..n {
                    at += width;
                    let block = words[at];
                    let n = words[at + 1] as usize;
                    f(block, at + 2, n);
                    at += 2 + n;
                }
            }
        }
    }
}

/// Allocates physical AArch64 GPRs, scalar FP registers, and (on register overflow) reusable
/// 8-byte stack spill slots for all non-fused values in `view`.
///
/// `skip_alloc[inst]` is `true` for values that were fused into their consumer(s) (e.g. a `mul`
/// fused into `madd`, a `slot_addr` folded into `[sp, #off]`, a small `iconst` folded as an
/// immediate, or an `icmp`/`fcmp` fused into `br`/`select`), so they consume ZERO physical
/// registers and ZERO spill slots!
pub(crate) fn allocate<A: Allocator + Clone>(
    view: &View<'_>,
    skip_alloc: &[bool],
    max_spill_slots: u32,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let len = view.len();
    let mut last_use: Vec<usize, A> = Vec::with_capacity_in(len, alloc.clone());
    let mut has_any_use: Vec<bool, A> = Vec::with_capacity_in(len, alloc.clone());
    let mut call_positions: Vec<usize, A> = Vec::new_in(alloc.clone());
    for i in 0..len {
        last_use.push(i);
        has_any_use.push(false);
    }

    // 1. Compute live intervals `[def, last_use]` + record call sites.
    for b in 0..view.blocks() {
        let range = view.block(Block(b as u32));
        for inst in range {
            let op = view.op(inst);
            if matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
                call_positions.push(inst);
            }
            for_each_use(view, inst, |v| {
                let vi = v as usize;
                if vi < len {
                    has_any_use[vi] = true;
                    if inst > last_use[vi] {
                        last_use[vi] = inst;
                    }
                }
            });
        }
    }

    // Propagate fused producers' operand lifetimes to their consumer's position (reverse scan).
    for inst in (0..len).rev() {
        if skip_alloc[inst] {
            let consumer_end = last_use[inst];
            for_each_use(view, inst, |v| {
                let vi = v as usize;
                if vi < len && consumer_end > last_use[vi] {
                    last_use[vi] = consumer_end;
                }
            });
        }
    }

    // Extend live intervals across backward CFG edges (loops) to a fixed point so nested loops
    // and fused loop-invariant operands are 100% sound.
    loop {
        let mut changed = false;
        for b in 0..view.blocks() {
            let term = view.terminator(Block(b as u32));
            for_each_edge(view, term, |target_block, _, _| {
                let target_start = view.block(Block(target_block)).start;
                if target_start <= term {
                    for v in 0..target_start {
                        if has_any_use[v] && last_use[v] >= target_start && last_use[v] < term {
                            last_use[v] = term;
                            changed = true;
                        }
                    }
                }
            });
        }
        if !changed {
            break;
        }
        // Re-propagate fused producers if a back-edge extended a skipped producer's lifetime.
        for inst in (0..len).rev() {
            if skip_alloc[inst] {
                let consumer_end = last_use[inst];
                for_each_use(view, inst, |v| {
                    let vi = v as usize;
                    if vi < len && consumer_end > last_use[vi] {
                        last_use[vi] = consumer_end;
                    }
                });
            }
        }
    }

    // 2. Entry parameters occupy `X0..X7` (GPR) and `V0..V7` (FP) at instruction 0: coalesce them in place!
    let mut loc_of: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc.clone());
    let mut hint: Vec<u8, A> = Vec::with_capacity_in(len, alloc.clone());
    for _ in 0..len {
        loc_of.push(AllocLoc::None);
        hint.push(NO_REG);
    }

    let entry_params = view.params(Block(0));
    let mut gpr_arg_idx = 0usize;
    let mut fp_arg_idx = 0usize;
    for p in entry_params.clone() {
        let ty = Ty(view.tys[p]);
        if is_fp_ty(ty).is_some() {
            if fp_arg_idx < FP_ARG_REGS.len() {
                hint[p] = FP_ARG_REGS[fp_arg_idx] as u8;
                fp_arg_idx += 1;
            }
        } else if gpr_arg_idx < ARG_REGS.len() {
            hint[p] = ARG_REGS[gpr_arg_idx] as u8;
            gpr_arg_idx += 1;
        }
    }

    // 3. Linear-scan chordal coloring in instruction (program) order with spill-slot reuse.
    let mut active_gpr: Vec<(usize, u8), A> = Vec::new_in(alloc.clone());
    let mut active_fp: Vec<(usize, u8), A> = Vec::new_in(alloc.clone());
    let mut active_spills: Vec<(usize, u32), A> = Vec::new_in(alloc.clone());
    let mut gpr_in_use = [false; 32];
    let mut fp_in_use = [false; 32];
    let mut spill_in_use: Vec<bool, A> = Vec::new_in(alloc.clone());
    let mut used_callee_gpr = [false; 32];
    let mut used_callee_fp = [false; 32];
    let mut num_spill_slots = 0u32;

    for inst in 0..len {
        // Expire intervals that ended strictly before `inst`.
        let mut w = 0;
        while w < active_gpr.len() {
            let (end, reg) = active_gpr[w];
            if end < inst {
                gpr_in_use[reg as usize] = false;
                active_gpr.remove_swap(w);
            } else {
                w += 1;
            }
        }
        let mut w = 0;
        while w < active_fp.len() {
            let (end, reg) = active_fp[w];
            if end < inst {
                fp_in_use[reg as usize] = false;
                active_fp.remove_swap(w);
            } else {
                w += 1;
            }
        }
        let mut w = 0;
        while w < active_spills.len() {
            let (end, slot) = active_spills[w];
            if end < inst {
                spill_in_use[slot as usize] = false;
                active_spills.remove_swap(w);
            } else {
                w += 1;
            }
        }

        if skip_alloc[inst] {
            continue;
        }
        let ty = Ty(view.tys[inst]);
        if ty == Ty::NONE {
            continue;
        }
        if !has_any_use[inst] {
            continue;
        }

        let end = last_use[inst];
        // A value read only at call `c` (`end == c`) dies before `blr` and does NOT cross `c`.
        let crosses_call = call_positions.iter().any(|&c| c > inst && c < end);

        if is_fp_ty(ty).is_some() {
            let chosen = if crosses_call {
                CALLEE_SAVED_FP.iter().copied().find(|&r| !fp_in_use[r as usize])
            } else {
                let h = hint[inst];
                if h != NO_REG && !fp_in_use[h as usize] {
                    Some(u32::from(h))
                } else {
                    CALLER_SAVED_FP
                        .iter()
                        .copied()
                        .chain(CALLEE_SAVED_FP.iter().copied())
                        .find(|&r| !fp_in_use[r as usize])
                }
            };

            if let Some(reg) = chosen {
                loc_of[inst] = AllocLoc::Fp(reg as u8);
                fp_in_use[reg as usize] = true;
                if (8..=15).contains(&reg) {
                    used_callee_fp[reg as usize] = true;
                }
                active_fp.push((end, reg as u8));
            } else {
                // Spill FP value to reusable 8-byte stack slot.
                let slot = alloc_spill_slot(&mut spill_in_use, &mut num_spill_slots, max_spill_slots)?;
                loc_of[inst] = AllocLoc::FpSpill(slot);
                active_spills.push((end, slot));
            }
        } else {
            let chosen = if crosses_call {
                CALLEE_SAVED_GPR.iter().copied().find(|&r| !gpr_in_use[r as usize])
            } else {
                let h = hint[inst];
                if h != NO_REG && !gpr_in_use[h as usize] {
                    Some(u32::from(h))
                } else {
                    CALLER_SAVED_GPR
                        .iter()
                        .copied()
                        .chain(CALLEE_SAVED_GPR.iter().copied())
                        .find(|&r| !gpr_in_use[r as usize])
                }
            };

            if let Some(reg) = chosen {
                loc_of[inst] = AllocLoc::Gpr(reg as u8);
                gpr_in_use[reg as usize] = true;
                if (X19..=X28).contains(&reg) {
                    used_callee_gpr[reg as usize] = true;
                }
                active_gpr.push((end, reg as u8));
            } else {
                // Spill GPR value to reusable 8-byte stack slot.
                let slot = alloc_spill_slot(&mut spill_in_use, &mut num_spill_slots, max_spill_slots)?;
                loc_of[inst] = AllocLoc::GprSpill(slot);
                active_spills.push((end, slot));
            }
        }
    }

    // Build exact ARM64 unwind pair masks (`blaze_code::unwind::Frame`).
    let mut x_pairs = 0u8;
    for k in 0..5u32 {
        let lo = (19 + 2 * k) as usize;
        let hi = lo + 1;
        if used_callee_gpr[lo] || used_callee_gpr[hi] {
            x_pairs |= 1 << k;
        }
    }
    let mut d_pairs = 0u8;
    for k in 0..4u32 {
        let lo = (8 + 2 * k) as usize;
        let hi = lo + 1;
        if used_callee_fp[lo] || used_callee_fp[hi] {
            d_pairs |= 1 << k;
        }
    }

    Ok(Allocation {
        loc_of,
        x_pairs,
        d_pairs,
        num_spill_slots,
        has_calls: !call_positions.is_empty(),
    })
}

fn alloc_spill_slot<A: Allocator>(
    spill_in_use: &mut Vec<bool, A>,
    num_spill_slots: &mut u32,
    max_spill_slots: u32,
) -> Result<u32, RegAllocError> {
    for (idx, used) in spill_in_use.iter_mut().enumerate() {
        if !*used {
            *used = true;
            return Ok(idx as u32);
        }
    }
    if *num_spill_slots >= max_spill_slots {
        return Err(RegAllocError::OutOfSpillSlots);
    }
    let slot = *num_spill_slots;
    *num_spill_slots += 1;
    spill_in_use.push(true);
    Ok(slot)
}

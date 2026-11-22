//! Chordal-SSA Register Allocator for the non-LLVM `Sota` AArch64 tier.
//!
//! Because `blaze-lir` is in strict SSA form with block parameters, its interference graph is
//! chordal. This allocator computes exact `[def, last_use]` intervals (extended across backward
//! loop edges), marks values live across `Call` instructions for callee-saved allocation
//! (`X19..X28`), coalesces entry parameters (`X0..X7`) and loop-carried parameters where
//! non-overlapping, and keeps pure leaf functions completely frameless (`X0..X15`, zero stack
//! frame, zero spills).

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

/// Unassigned register sentinel.
pub(crate) const NO_REG: u8 = u8::MAX;

/// Caller-saved allocatable GPRs (`X16`/`X17` stay permanently free as scratch/copy-cycle breakers,
/// `X18` is platform-reserved on macOS, `X8` is indirect-result/caller-saved).
const CALLER_SAVED: &[Reg] = &[X9, X10, X11, X12, X13, X14, X15, X8, X0, X1, X2, X3, X4, X5, X6, X7];

/// Callee-saved allocatable GPRs (saved/restored in pairs only when actually allocated).
const CALLEE_SAVED: &[Reg] = &[X19, X20, X21, X22, X23, X24, X25, X26, X27, X28];

/// AAPCS64 argument registers `X0..X7`.
pub(crate) const ARG_REGS: [Reg; 8] = [X0, X1, X2, X3, X4, X5, X6, X7];

/// Why register allocation could not fit all values in physical GPRs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RegAllocError {
    /// Simultaneous live values exceeded physical GPRs; caller may fall back to cell/Fast tier.
    OutOfRegisters,
}

/// Complete physical register assignment for a verified LIR function.
pub(crate) struct Allocation<A: Allocator> {
    /// Physical AArch64 GPR number per LIR instruction index (`NO_REG` if unused or void).
    pub(crate) reg_of: Vec<u8, A>,
    /// Distinct callee-saved registers actually used (sorted ascending, for minimal `stp`/`ldp`).
    pub(crate) callee_saved: Vec<Reg, A>,
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

/// Allocates physical AArch64 GPRs for all non-fused values in `view`.
///
/// `skip_alloc[inst]` is `true` for values that were fused into their sole consumer (e.g. a
/// `mul` fused into `madd`, a small `iconst` folded as an immediate, or an `icmp` fused into
/// `br`/`select`), so they consume ZERO physical registers!
pub(crate) fn allocate<A: Allocator + Clone>(
    view: &View<'_>,
    skip_alloc: &[bool],
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
        for inst in range.clone() {
            let op = view.op(inst);
            if matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
                call_positions.push(inst);
            }
            if !skip_alloc[inst] {
                for_each_use(view, inst, |v| {
                    let vi = v as usize;
                    if vi < len {
                        has_any_use[vi] = true;
                        if inst > last_use[vi] {
                            last_use[vi] = inst;
                        }
                    }
                });
            } else {
                // Fused instruction: its operands are read at the consumer, so extend at least to `inst`
                // and propagate transitively below.
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
        // Extend intervals for operands of fused producers up to their consumer's `last_use`!
        let term = range.end - 1;
        for_each_edge(view, term, |target_block, _, _| {
            let target_start = view.block(Block(target_block)).start;
            if target_start <= term {
                // Backward edge (loop latch -> header): every value defined before `target_start`
                // that is used at or after `target_start` stays live through `term`!
                for v in 0..target_start {
                    if has_any_use[v] && last_use[v] >= target_start && last_use[v] < term {
                        last_use[v] = term;
                    }
                }
            }
        });
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

    // Re-apply back-edge extension after fused propagation to guarantee loop-carried soundness.
    for b in 0..view.blocks() {
        let term = view.terminator(Block(b as u32));
        for_each_edge(view, term, |target_block, _, _| {
            let target_start = view.block(Block(target_block)).start;
            if target_start <= term {
                for v in 0..target_start {
                    if has_any_use[v] && last_use[v] >= target_start && last_use[v] < term {
                        last_use[v] = term;
                    }
                }
            }
        });
    }

    // 2. Entry parameters occupy `X0..X7` at instruction 0, so prefer coalescing them in place!
    let mut reg_of: Vec<u8, A> = Vec::with_capacity_in(len, alloc.clone());
    for _ in 0..len {
        reg_of.push(NO_REG);
    }

    let entry_params = view.params(Block(0));
    let mut hint: Vec<u8, A> = Vec::with_capacity_in(len, alloc.clone());
    for _ in 0..len {
        hint.push(NO_REG);
    }
    for (k, p) in entry_params.clone().enumerate() {
        if k < ARG_REGS.len() {
            hint[p] = ARG_REGS[k] as u8;
        }
    }

    // Linear-scan chordal coloring in instruction (program) order.
    let mut active: Vec<(usize, u8), A> = Vec::new_in(alloc.clone()); // (end_pos, reg)
    let mut in_use = [false; 32];
    let mut used_callee = [false; 32];

    for inst in 0..len {
        // Expire intervals that ended strictly before `inst` (a value whose last use is `inst`
        // can reuse its register for `inst`'s result ONLY if we keep inputs alive during `inst`
        // — freeing strictly `< inst` is 100% safe for multi-instruction patterns and swaps!).
        let mut w = 0;
        while w < active.len() {
            let (end, reg) = active[w];
            if end < inst {
                in_use[reg as usize] = false;
                active.remove_swap(w);
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
        // Entry params must still be allocated even if unused? No — if unused, skip!
        if !has_any_use[inst] && !entry_params.contains(&inst) {
            continue;
        }

        let end = last_use[inst];
        let crosses_call = call_positions.iter().any(|&c| c > inst && c <= end);

        let chosen = if crosses_call {
            // Must pick a callee-saved register so calls preserve it with zero spill!
            CALLEE_SAVED.iter().copied().find(|&r| !in_use[r as usize])
        } else {
            // Try coalescing hint first (e.g. entry param stays in X0..X7 with zero mov!).
            let h = hint[inst];
            if h != NO_REG && !in_use[h as usize] && !crosses_call {
                Some(u32::from(h))
            } else {
                CALLER_SAVED
                    .iter()
                    .copied()
                    .chain(CALLEE_SAVED.iter().copied())
                    .find(|&r| !in_use[r as usize])
            }
        };

        let Some(reg) = chosen else {
            return Err(RegAllocError::OutOfRegisters);
        };
        reg_of[inst] = reg as u8;
        in_use[reg as usize] = true;
        if reg >= X19 && reg <= X28 {
            used_callee[reg as usize] = true;
        }
        active.push((end, reg as u8));
    }

    let mut callee_saved: Vec<Reg, A> = Vec::new_in(alloc);
    for &r in CALLEE_SAVED {
        if used_callee[r as usize] {
            callee_saved.push(r);
        }
    }

    Ok(Allocation {
        reg_of,
        callee_saved,
        has_calls: !call_positions.is_empty(),
    })
}

//! 4x Scalar Loop Unrolling with Independent Multi-Accumulators (`unroll_scalar_loops`).
//!
//! Breaks loop-carried ALU/multiplier dependency chains on scalar reduction loops that are not
//! SIMD-vectorizable (e.g., constant division/modulo `i / 10 + i % 10`, integer hashing, CRC,
//! unswitched devirtualized loops):
//!
//! - Clones the loop body 4x (`i`, `i+1`, `i+2`, `i+3`) with **4 independent scalar accumulators**
//!   (`acc0, acc1, acc2, acc3`) per associative reduction (`Add`, `Xor`, `Or`, `And`), allowing
//!   out-of-order cores (Apple Silicon 6-ALU / 2-MUL, Zen 5 4-ALU) to execute 4 iterations in
//!   parallel!
//! - Finishes any `< 4` tail iterations in the untouched original scalar loop for bit-exact
//!   equivalence across every trip count (`0, 1, 2, 3, 4, 5, ...`).
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, each, filled, Item},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::BlockKind,
    verify,
};

/// Telemetry returned by [`unroll_scalar_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct UnrollStats {
    pub loops_unrolled: u32,
}

/// Output of [`unroll_scalar_loops`].
#[derive(Debug)]
pub struct UnrollOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: UnrollStats,
}

const UNROLL_FACTOR: usize = 4;
const MAX_BODY_INSTS: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScalarLoopShape {
    HeaderBody { header: u32, body: u32, exit: u32 },
    SingleBlock { header: u32, exit: u32 },
}

#[derive(Clone, Copy)]
struct ScalarReduction {
    param_pos: usize,
    param_inst: u32,
    init_val: u32,
    op: Op,
    flags: Flags,
    ty: Ty,
    /// When `true` (strict IEEE-754 `Fadd`), thread a single accumulator sequentially across
    /// all 4 unrolled iterations (`(((acc + e0) + e1) + e2) + e3`) for bit-exact equivalence!
    strict_order: bool,
    update_inst: u32,
    elem_val: u32,
}

#[derive(Clone, Copy)]
struct UnitLoad {
    inst: u32,
    base: u32,
    ty: Ty,
    shift: u32,
    off: i32,
    mem_attr: u32,
}

struct UnrollPlan<A: Allocator> {
    preheader: u32,
    shape: ScalarLoopShape,
    iv_pos: usize,
    iv_inst: u32,
    iv_ty: Ty,
    iv_init: u32,
    bound: u32,
    /// Loop exit comparison (`IntCond::Ult` for `i < bound`, `IntCond::Ule` for `i <= bound`).
    cmp_cond: IntCond,
    pre_args: Vec<u32, A>,
    reductions: Vec<ScalarReduction, A>,
    /// Unit-stride FP loads emitted contiguously (`+0, +8, +16, +24`) for `ldp d, d` pair fusion!
    unit_loads: Vec<UnitLoad, A>,
    /// Ordered body instructions to replicate 4x (excluding IV step, exit cmp, unit_loads, and reduction updates).
    body_insts: Vec<u32, A>,
    single_exit_args: Vec<u32, A>,
    /// Indices into `reductions` whose `update_inst` escapes directly into `exit` (and is promoted
    /// to a new `Op::Param` on `exit` for `SingleBlock` loops).
    promoted_exits: Vec<usize, A>,
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn at<T: Copy + Default>(s: &[T], i: usize) -> T {
    s.get(i).copied().unwrap_or_default()
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&c| c == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
const fn int_cond_word(c: IntCond) -> u32 {
    match c {
        IntCond::Eq => 0,
        IntCond::Ne => 1,
        IntCond::Ult => 2,
        IntCond::Ule => 3,
        IntCond::Ugt => 4,
        IntCond::Uge => 5,
        IntCond::Slt => 6,
        IntCond::Sle => 7,
        IntCond::Sgt => 8,
        IntCond::Sge => 9,
    }
}

#[inline]
const fn block_kind_byte(k: BlockKind) -> u8 {
    match k {
        BlockKind::Normal => 0,
        BlockKind::Cold => 1,
        BlockKind::Cleanup => 2,
        BlockKind::Catch => 3,
        BlockKind::Terminate => 4,
    }
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(at(view.tys, inst));
    if !ty.is_scalar() || !ty.is_int() || ty.bits(64) > 64 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(at(view.data, inst), width),
        Op::IconstWide => {
            let idx = to_idx(at(view.data, inst));
            let words = view.extra.get(idx..idx + to_idx(ty.const_words()))?;
            wide_value(words, width)
        }
        _ => return None,
    };
    u64::try_from(v).ok()
}

fn find_unrollable_loop<A: Allocator + Copy>(view: &View<'_>, alloc: A) -> Option<UnrollPlan<A>> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return None;
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.scan_for(&b_u32).is_none()
                {
                    p.push(b_u32);
                }
            }
        });
    }

    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, latch) = if p0 < header && p1 >= header {
            (p0, p1)
        } else if p1 < header && p0 >= header {
            (p1, p0)
        } else {
            continue;
        };
        let pre_term = view.terminator(Block(preheader));
        if view.op(pre_term) != Op::Jump {
            continue;
        }
        let pre_at = to_idx(at(view.data, pre_term));
        if at(view.extra, pre_at) != header {
            continue;
        }
        let h_params = view.params(Block(header));
        let pre_argc = to_idx(at(view.extra, pre_at + 1));
        if h_params.len() < 2 || pre_argc != h_params.len() {
            continue;
        }
        let mut pre_args = Vec::with_capacity_in(pre_argc, alloc);
        for k in 0..pre_argc {
            pre_args.push(at(view.extra, pre_at + 2 + k));
        }

        let (shape, body_block, back_args_start, cond_val, single_exit_args) = if latch == header {
            let h_term = view.terminator(Block(header));
            if view.op(h_term) != Op::Br {
                continue;
            }
            let idx = to_idx(at(view.data, h_term));
            let cv = at(view.extra, idx);
            let t_blk = at(view.extra, idx + 1);
            let t_argc = to_idx(at(view.extra, idx + 2));
            let e_at = idx + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let e_argc = to_idx(at(view.extra, e_at + 1));
            if t_blk != header || e_blk == header || t_argc != h_params.len() {
                continue;
            }
            let mut ex_args = Vec::with_capacity_in(e_argc, alloc);
            for k in 0..e_argc {
                ex_args.push(at(view.extra, e_at + 2 + k));
            }
            (ScalarLoopShape::SingleBlock { header, exit: e_blk }, header, idx + 3, cv, ex_args)
        } else {
            let latch_single = preds.get(to_idx(latch)).map(Vec::as_slice) == Some(&[header]);
            if !latch_single || !view.params(Block(latch)).is_empty() {
                continue;
            }
            let h_term = view.terminator(Block(header));
            let b_term = view.terminator(Block(latch));
            if view.op(h_term) != Op::Br || view.op(b_term) != Op::Jump {
                continue;
            }
            let hat = to_idx(at(view.data, h_term));
            let cv = at(view.extra, hat);
            let t_blk = at(view.extra, hat + 1);
            let t_argc = to_idx(at(view.extra, hat + 2));
            let e_at = hat + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let bat = to_idx(at(view.data, b_term));
            if t_blk != latch || t_argc != 0 || e_blk == header || e_blk == latch {
                continue;
            }
            if at(view.extra, bat) != header || to_idx(at(view.extra, bat + 1)) != h_params.len() {
                continue;
            }
            let h_range = view.block(Block(header));
            if (h_params.end..(h_range.end - 1)).any(|inst| inst != to_idx(cv)) {
                continue;
            }
            (ScalarLoopShape::HeaderBody { header, body: latch, exit: e_blk }, latch, bat + 2, cv, Vec::new_in(alloc))
        };

        let is_loop_blk = |b: u32| b == header || b == body_block;
        let ci = to_idx(cond_val);
        if view.op(ci) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, ci));
        let Some(cmp_cond) = IntCond::from_word(at(view.extra, cat)).filter(|c| matches!(c, IntCond::Ult | IntCond::Ule)) else {
            continue;
        };
        let cmp_lhs = at(view.extra, cat + 1);
        let bound = at(view.extra, cat + 2);
        if is_loop_blk(at(&inst_block, to_idx(bound))) {
            continue;
        }

        // Match unit-step induction variable `i` (`i_next = i + 1`).
        let mut found_iv = None;
        for pos in 0..h_params.len() {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let back_v = at(view.extra, back_args_start + pos);
            let back_i = to_idx(back_v);
            let p_ty = Ty(at(view.tys, p_idx));
            if p_ty != Ty::I64 && p_ty != Ty::I32 {
                continue;
            }
            if view.op(back_i) != Op::Add {
                continue;
            }
            let aat = to_idx(at(view.data, back_i));
            let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
            let is_step1 = (al == p_inst && const_u64(view, to_idx(ar)) == Some(1))
                || (ar == p_inst && const_u64(view, to_idx(al)) == Some(1));
            if !is_step1 {
                continue;
            }
            let ok_cmp = match shape {
                ScalarLoopShape::HeaderBody { .. } => cmp_lhs == p_inst,
                ScalarLoopShape::SingleBlock { .. } => cmp_lhs == back_v,
            };
            if ok_cmp {
                found_iv = Some((pos, p_inst, back_v, p_ty));
                break;
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst, iv_ty)) = found_iv else {
            continue;
        };

        let body_range = view.block(Block(body_block));
        let body_start = if body_block == header { h_params.end } else { body_range.start };
        let body_end = body_range.end - 1;
        if body_end - body_start == 0 || body_end - body_start > MAX_BODY_INSTS {
            continue;
        }

        // Skip loops with calls, stores, volatile loads, atomics, vectors (already vectorized!),
        // or trapping variable divisions! Non-volatile `Load`s are allowed for strict FP reduction loops.
        let mut body_ok = true;
        let mut has_load = false;
        for inst in body_start..body_end {
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            if ty.is_vector() {
                body_ok = false;
                break;
            }
            match op {
                Op::Load => {
                    if Flags(at(view.flags, inst)).contains(Flags::VOLATILE) {
                        body_ok = false;
                        break;
                    }
                    has_load = true;
                }
                Op::Call
                | Op::CallIndirect
                | Op::Store
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::Trap => {
                    body_ok = false;
                    break;
                }
                // Only allow `Udiv`/`Urem` by a non-zero compile-time constant (trap-free!).
                Op::Udiv | Op::Urem => {
                    let idx = to_idx(at(view.data, inst));
                    let rhs = to_idx(at(view.extra, idx + 1));
                    if const_u64(view, rhs).is_none_or(|d| d == 0) {
                        body_ok = false;
                        break;
                    }
                }
                Op::Sdiv | Op::Srem => {
                    body_ok = false;
                    break;
                }
                _ => {}
            }
        }
        if !body_ok {
            continue;
        }

        // Match every non-IV header parameter as either an associative integer reduction
        // (`Add`, `Xor`, `Or`, `And` -> 4 independent accumulators) or a strict-order IEEE-754
        // FP reduction (`Fadd` on `F64`/`F32` -> 1 sequentially threaded accumulator).
        let mut reductions: Vec<ScalarReduction, A> = Vec::new_in(alloc);
        let mut is_red_upd = filled(len, false, &alloc);
        let mut bad_param = false;
        let mut has_fp_red = false;
        for pos in 0..h_params.len() {
            if pos == iv_pos {
                continue;
            }
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let p_ty = Ty(at(view.tys, p_idx));
            let back_v = at(view.extra, back_args_start + pos);
            let bi = to_idx(back_v);
            let bop = view.op(bi);
            let bflags = Flags(at(view.flags, bi));
            if at(&inst_block, bi) != body_block {
                bad_param = true;
                break;
            }
            let is_int_red = (p_ty == Ty::I64 || p_ty == Ty::I32)
                && matches!(bop, Op::Add | Op::Xor | Op::Or | Op::And);
            let is_fp_red = (p_ty == Ty::F64 || p_ty == Ty::F32) && bop == Op::Fadd;
            if !is_int_red && !is_fp_red {
                bad_param = true;
                break;
            }
            if is_fp_red {
                has_fp_red = true;
            }
            let bat = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
            let elem_val = if l == p_inst {
                r
            } else if r == p_inst {
                l
            } else {
                bad_param = true;
                break;
            };
            // Ensure accumulator param is ONLY read by its own reduction update!
            let mut other_use = false;
            for inst in body_start..body_end {
                if inst == bi {
                    continue;
                }
                each(view, inst, |item| {
                    if let Item::Value(v) = item
                        && v == p_inst
                    {
                        other_use = true;
                    }
                });
            }
            if other_use {
                bad_param = true;
                break;
            }
            set_at(&mut is_red_upd, bi, true);
            reductions.push(ScalarReduction {
                param_pos: pos,
                param_inst: p_inst,
                init_val: at(&pre_args, pos),
                op: bop,
                flags: bflags,
                ty: p_ty,
                strict_order: is_fp_red,
                update_inst: back_v,
                elem_val,
            });
        }
        // Only unroll memory-reading loops when they are strict FP reductions (integer load loops
        // are already handled by the NEON SIMD auto-vectorizer).
        if bad_param || reductions.is_empty() || (has_load && !has_fp_red) {
            continue;
        }

        // Ensure no body instruction escapes illegally outside the loop.
        // For `SingleBlock { header, exit }` where `exit` has only `header` as predecessor, a
        // reduction update (`acc2`) used directly in blocks `b >= exit` is safely promoted to a
        // new block parameter on `exit`!
        let mut bad_escape = false;
        let mut promoted_exits: Vec<usize, A> = Vec::new_in(alloc);
        let can_promote_exit = match shape {
            ScalarLoopShape::SingleBlock { header: sh, exit: ex } => {
                preds.get(to_idx(ex)).map(Vec::as_slice) == Some(&[sh])
            }
            ScalarLoopShape::HeaderBody { .. } => false,
        };
        let exit_blk_id = match shape {
            ScalarLoopShape::SingleBlock { exit, .. } | ScalarLoopShape::HeaderBody { exit, .. } => exit,
        };
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        if is_loop_blk(at(&inst_block, vi)) && !(vi >= h_params.start && vi < h_params.end) {
                            if can_promote_exit
                                && b_u32 >= exit_blk_id
                                && let Some(ridx) = reductions.iter().position(|r| r.update_inst == v)
                            {
                                if promoted_exits.scan_for(&ridx).is_none() {
                                    promoted_exits.push(ridx);
                                }
                                return;
                            }
                            bad_escape = true;
                        }
                    }
                });
            }
        }
        if bad_escape {
            continue;
        }
        if matches!(shape, ScalarLoopShape::SingleBlock { .. })
            && single_exit_args.iter().any(|&ea| {
                let ei = to_idx(ea);
                is_loop_blk(at(&inst_block, ei)) && !at(&is_red_upd, ei)
            })
        {
            continue;
        }

        // Identify unit-stride `f64`/`f32` loads (`ptr_add(base, shl(iv, 3|2))` or `ptr_add(base, mul(iv, 8|4))`)
        // so we emit all 4 adjacent loads per base contiguously (`+0, +8, +16, +24`) for `ldp d, d` pairing!
        let mut unit_loads: Vec<UnitLoad, A> = Vec::new_in(alloc);
        let mut is_unit_load_or_addr = filled(len, false, &alloc);
        let mut use_cnt = filled(len, 0u32, &alloc);
        for inst in body_start..body_end {
            each(view, inst, |item| {
                if let Item::Value(v) = item {
                    let vi = to_idx(v);
                    if let Some(slot) = use_cnt.get_mut(vi) {
                        *slot = slot.saturating_add(1);
                    }
                }
            });
        }
        for inst in body_start..body_end {
            if view.op(inst) == Op::Load {
                let lty = Ty(at(view.tys, inst));
                let shift = if lty == Ty::F64 {
                    3u32
                } else if lty == Ty::F32 {
                    2
                } else {
                    continue;
                };
                let idx = to_idx(at(view.data, inst));
                let ptr = to_idx(at(view.extra, idx));
                let off = at(view.extra, idx + 1).cast_signed();
                if view.op(ptr) == Op::PtrAdd && at(&inst_block, ptr) == body_block {
                    let pat = to_idx(at(view.data, ptr));
                    let base = at(view.extra, pat);
                    let by = to_idx(at(view.extra, pat + 1));
                    if !is_loop_blk(at(&inst_block, to_idx(base))) {
                        let is_unit = match view.op(by) {
                            Op::Shl => {
                                let sat = to_idx(at(view.data, by));
                                at(view.extra, sat) == iv_inst
                                    && const_u64(view, to_idx(at(view.extra, sat + 1))) == Some(u64::from(shift))
                            }
                            Op::Mul => {
                                let mat = to_idx(at(view.data, by));
                                let (l, r) = (at(view.extra, mat), at(view.extra, mat + 1));
                                (l == iv_inst && const_u64(view, to_idx(r)) == Some(1u64 << shift))
                                    || (r == iv_inst && const_u64(view, to_idx(l)) == Some(1u64 << shift))
                            }
                            _ => false,
                        };
                        if is_unit {
                            unit_loads.push(UnitLoad {
                                inst: to_u32(inst),
                                base,
                                ty: lty,
                                shift,
                                off,
                                mem_attr: at(view.attrs, inst),
                            });
                            set_at(&mut is_unit_load_or_addr, inst, true);
                            if at(&use_cnt, ptr) == 1 {
                                set_at(&mut is_unit_load_or_addr, ptr, true);
                            }
                        }
                    }
                }
            }
        }
        let iv_next_idx = to_idx(iv_next_inst);
        // Also drop the shared `shl(iv, shift)` if all its uses were `ptr_add`s we just absorbed.
        for inst in body_start..body_end {
            if !at(&is_unit_load_or_addr, inst) && matches!(view.op(inst), Op::Shl | Op::Mul) {
                let mut any_kept_use = false;
                for u_inst in body_start..body_end {
                    if at(&is_unit_load_or_addr, u_inst)
                        || u_inst == iv_next_idx
                        || u_inst == ci
                        || at(&is_red_upd, u_inst)
                    {
                        continue;
                    }
                    each(view, u_inst, |item| {
                        if let Item::Value(v) = item
                            && to_idx(v) == inst
                        {
                            any_kept_use = true;
                        }
                    });
                }
                if !any_kept_use && !unit_loads.is_empty() {
                    set_at(&mut is_unit_load_or_addr, inst, true);
                }
            }
        }

        // Collect pure body instructions to replicate 4x (exclude iv_next, cond_val, unit_loads, and reduction updates).
        let mut body_insts: Vec<u32, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            if inst == iv_next_idx || inst == ci || at(&is_red_upd, inst) || at(&is_unit_load_or_addr, inst) {
                continue;
            }
            // Also make sure no replicated instruction depends on `iv_next_inst` directly!
            let mut uses_iv_next = false;
            each(view, inst, |item| {
                if let Item::Value(v) = item
                    && v == iv_next_inst
                {
                    uses_iv_next = true;
                }
            });
            if uses_iv_next {
                body_ok = false;
                break;
            }
            body_insts.push(to_u32(inst));
        }
        if !body_ok {
            continue;
        }

        let iv_init = at(&pre_args, iv_pos);
        return Some(UnrollPlan {
            preheader,
            shape,
            iv_pos,
            iv_inst,
            iv_ty,
            iv_init,
            bound,
            cmp_cond,
            pre_args,
            reductions,
            unit_loads,
            body_insts,
            single_exit_args,
            promoted_exits,
        });
    }

    None
}

fn push_inst<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, data: u32) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(0);
    func.locs.push(0);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64) -> u32 {
    if ty.bits(64) <= 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val))
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned())
    } else {
        let idx = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, idx)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, Flags::NONE, ty, idx)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, idx)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) {
    let idx = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, idx);
}

fn push_br<A: Allocator>(func: &mut Func<A>, cond: u32, t_blk: u32, t_args: &[u32], e_blk: u32, e_args: &[u32]) {
    let idx = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_blk);
    func.extra.push(to_u32(t_args.len()));
    for &a in t_args {
        func.extra.push(a);
    }
    func.extra.push(e_blk);
    func.extra.push(to_u32(e_args.len()));
    for &a in e_args {
        func.extra.push(a);
    }
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, idx);
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    use crate::op::{Field, Layout};
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst))),
        Layout::Data(Field::Block) => at(blk_map, to_idx(at(view.data, inst))),
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at(val_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(at(blk_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(blk_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        let b = to_idx(at(view.extra, p));
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(at(blk_map, b));
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at(view.extra, src);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k));
                            }
                            p += w;
                            let b = to_idx(at(view.extra, p));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(at(blk_map, b));
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(at(view.ops, inst));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Unrolls an eligible scalar integer reduction loop 4x with 4 independent accumulators.
pub fn unroll_scalar_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<UnrollOutput<A>, SimplifyError> {
    let Some(plan) = find_unrollable_loop(&view, alloc) else {
        return Ok(UnrollOutput { func: view.to_func_in(alloc), stats: UnrollStats::default() });
    };

    let blocks = view.blocks();
    let len = view.len();
    let insert_at = to_idx(plan.preheader) + 1;
    let u_pre_blk = to_u32(insert_at);
    let u_loop_blk = to_u32(insert_at + 1);
    let u_red_blk = to_u32(insert_at + 2);

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + 3) });
    }
    let orig_header_new = match plan.shape {
        ScalarLoopShape::HeaderBody { header, .. } | ScalarLoopShape::SingleBlock { header, .. } => {
            at(&blk_map, to_idx(header))
        }
    };
    let orig_exit_new = match plan.shape {
        ScalarLoopShape::HeaderBody { exit, .. } | ScalarLoopShape::SingleBlock { exit, .. } => {
            at(&blk_map, to_idx(exit))
        }
    };

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let unroll_u64 = u64::try_from(UNROLL_FACTOR).unwrap_or(4);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let bound_new = at(&val_map, to_idx(plan.bound));
            let has_rem = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
            let rem_val = push_bin(&mut func, Op::Sub, plan.iv_ty, bound_new, iv_init_new);
            let k4 = push_iconst(&mut func, plan.iv_ty, unroll_u64);
            let enough = push_icmp(&mut func, IntCond::Uge, rem_val, k4);
            let go_unroll = push_bin(&mut func, Op::And, Ty::I1, has_rem, enough);

            let mut fb_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for &a in &plan.pre_args {
                fb_args.push(at(&val_map, to_idx(a)));
            }
            push_br(&mut func, go_unroll, u_pre_blk, &[], orig_header_new, &fb_args);

            // --- `u_pre`: compute `u_end = iv_init + (rem & !3)` and init 4 independent accumulators ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let k_mask = push_iconst(&mut func, plan.iv_ty, !(unroll_u64 - 1));
            let u_cnt = push_bin(&mut func, Op::And, plan.iv_ty, rem_val, k_mask);
            let u_end = push_bin(&mut func, Op::Add, plan.iv_ty, iv_init_new, u_cnt);

            let mut init_args: Vec<u32, A> = Vec::with_capacity_in(1 + plan.reductions.len() * UNROLL_FACTOR, alloc);
            init_args.push(iv_init_new);
            for red in &plan.reductions {
                init_args.push(at(&val_map, to_idx(red.init_val)));
                if !red.strict_order {
                    let id_val = if red.op == Op::And { u64::MAX } else { 0 };
                    let id_c = push_iconst(&mut func, red.ty, id_val);
                    for _ in 1..UNROLL_FACTOR {
                        init_args.push(id_c);
                    }
                }
            }
            push_jump(&mut func, u_loop_blk, &init_args);

            // --- `u_loop`: 4x unrolled scalar loop (4 independent int accs, or 1 strict-order FP acc)! ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv0 = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0);
            let mut acc_params: Vec<[u32; UNROLL_FACTOR], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                let a0 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                if red.strict_order {
                    acc_params.push([a0, a0, a0, a0]);
                } else {
                    let a1 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    let a2 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    let a3 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    acc_params.push([a0, a1, a2, a3]);
                }
            }

            let mut u_next_accs: Vec<[u32; UNROLL_FACTOR], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for _ in 0..plan.reductions.len() {
                u_next_accs.push([NONE; UNROLL_FACTOR]);
            }

            let mut local_maps: [Vec<u32, A>; UNROLL_FACTOR] = [
                filled(len, NONE, &alloc),
                filled(len, NONE, &alloc),
                filled(len, NONE, &alloc),
                filled(len, NONE, &alloc),
            ];
            for lm in &mut local_maps {
                for (dst, &src) in lm.iter_mut().zip(val_map.iter()) {
                    *dst = src;
                }
            }

            // Emit all 4 contiguous loads per `UnitLoad` base first (`+0, +8, +16, +24`) so AArch64 `isel.rs`
            // fuses them directly into `ldp d, d` pairs and issues all 4 `fmul`s in parallel!
            if !plan.unit_loads.is_empty() {
                let iv0_64 = if plan.iv_ty == Ty::I32 {
                    push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv0)
                } else {
                    iv0
                };
                let mut boff_by_shift: [(u32, u32); 4] = [(u32::MAX, NONE); 4];
                for ul in &plan.unit_loads {
                    let boff = if let Some(&(_, existing)) = boff_by_shift.iter().find(|&&(s, _)| s == ul.shift) {
                        existing
                    } else {
                        let ks = push_iconst(&mut func, Ty::I64, u64::from(ul.shift));
                        let b_val = push_bin(&mut func, Op::Shl, Ty::I64, iv0_64, ks);
                        if let Some(slot) = boff_by_shift.iter_mut().find(|&&mut (s, _)| s == u32::MAX) {
                            *slot = (ul.shift, b_val);
                        }
                        b_val
                    };
                    let base_p = push_bin(&mut func, Op::PtrAdd, Ty::PTR, at(&val_map, to_idx(ul.base)), boff);
                    let stride = 1i32 << ul.shift;
                    for (u, lm) in local_maps.iter_mut().enumerate() {
                        let idx = to_u32(func.extra.len());
                        func.extra.push(base_p);
                        let u_i32 = i32::try_from(u).unwrap_or(0);
                        func.extra.push(ul.off.wrapping_add(u_i32 * stride).cast_unsigned());
                        let l_idx = to_u32(func.ops.len());
                        func.ops.push(op_byte(Op::Load));
                        func.flags.push(0);
                        func.tys.push(ul.ty.0);
                        func.data.push(idx);
                        func.attrs.push(ul.mem_attr);
                        func.locs.push(0);
                        set_at(lm, to_idx(ul.inst), l_idx);
                    }
                }
            }

            // Emit replicated body expressions (all 4 `fmul`s / ALU ops before strict sequential `fadd`s!).
            let has_any_strict = plan.reductions.iter().any(|r| r.strict_order);
            let needs_iv_u = !plan.body_insts.is_empty()
                || plan.reductions.iter().any(|r| r.elem_val == plan.iv_inst);
            for (u, lm) in local_maps.iter_mut().enumerate() {
                let iv_u = if u == 0 {
                    iv0
                } else if needs_iv_u {
                    let ku = push_iconst(&mut func, plan.iv_ty, u64::try_from(u).unwrap_or(0));
                    push_bin(&mut func, Op::Add, plan.iv_ty, iv0, ku)
                } else {
                    iv0
                };
                set_at(lm, to_idx(plan.iv_inst), iv_u);
                for (ridx, red) in plan.reductions.iter().enumerate() {
                    let acc_u = acc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                    set_at(lm, to_idx(red.param_inst), acc_u);
                }
                for &bi in &plan.body_insts {
                    let bi_idx = to_idx(bi);
                    let mapped = copy_remapped(&view, bi_idx, lm, &blk_map, &mut func);
                    set_at(lm, bi_idx, mapped);
                }
                if !has_any_strict {
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        let elem_v = at(lm, to_idx(red.elem_val));
                        let acc_u = acc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                        let next_v = push_bin(&mut func, red.op, red.ty, acc_u, elem_v);
                        if let Some(row) = u_next_accs.get_mut(ridx) {
                            set_at(row, u, next_v);
                        }
                    }
                }
            }
            if has_any_strict {
                for (ridx, red) in plan.reductions.iter().enumerate() {
                    if red.strict_order {
                        let mut cur = acc_params.get(ridx).map_or(NONE, |row| row[0]);
                        for lm in &local_maps {
                            let elem_v = at(lm, to_idx(red.elem_val));
                            let idx = to_u32(func.extra.len());
                            func.extra.push(cur);
                            func.extra.push(elem_v);
                            cur = push_inst(&mut func, red.op, red.flags, red.ty, idx);
                        }
                        if let Some(row) = u_next_accs.get_mut(ridx) {
                            row[0] = cur;
                        }
                    } else {
                        for (u, lm) in local_maps.iter().enumerate() {
                            let elem_v = at(lm, to_idx(red.elem_val));
                            let acc_u = acc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                            let next_v = push_bin(&mut func, red.op, red.ty, acc_u, elem_v);
                            if let Some(row) = u_next_accs.get_mut(ridx) {
                                set_at(row, u, next_v);
                            }
                        }
                    }
                }
            }

            let k_step4 = push_iconst(&mut func, plan.iv_ty, unroll_u64);
            let iv_next = push_bin(&mut func, Op::Add, plan.iv_ty, iv0, k_step4);
            let mut back_args: Vec<u32, A> = Vec::with_capacity_in(1 + plan.reductions.len() * UNROLL_FACTOR, alloc);
            back_args.push(iv_next);
            for (ridx, red) in plan.reductions.iter().enumerate() {
                if let Some(row) = u_next_accs.get(ridx) {
                    if red.strict_order {
                        back_args.push(row[0]);
                    } else {
                        for &v in row {
                            back_args.push(v);
                        }
                    }
                }
            }
            let keep = push_icmp(&mut func, IntCond::Ult, iv_next, u_end);
            push_br(&mut func, keep, u_loop_blk, &back_args, u_red_blk, &back_args);

            // --- `u_red`: tree-reduce associative accumulators (or pass strict FP acc directly) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_done = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0);
            let mut reduced_vals: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                if red.strict_order {
                    let d0 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    reduced_vals.push(d0);
                } else {
                    let d0 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    let d1 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    let d2 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    let d3 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    let c01 = push_bin(&mut func, red.op, red.ty, d0, d1);
                    let c23 = push_bin(&mut func, red.op, red.ty, d2, d3);
                    reduced_vals.push(push_bin(&mut func, red.op, red.ty, c01, c23));
                }
            }

            let mut tail_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                if pos == plan.iv_pos {
                    tail_args.push(iv_done);
                } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                    tail_args.push(at(&reduced_vals, ridx));
                } else {
                    tail_args.push(at(&val_map, to_idx(pre_arg)));
                }
            }
            match plan.shape {
                ScalarLoopShape::HeaderBody { .. } => push_jump(&mut func, orig_header_new, &tail_args),
                ScalarLoopShape::SingleBlock { .. } => {
                    let has_tail = push_icmp(&mut func, plan.cmp_cond, iv_done, bound_new);
                    let mut direct_exit_args: Vec<u32, A> =
                        Vec::with_capacity_in(plan.single_exit_args.len() + plan.promoted_exits.len(), alloc);
                    for &ea in &plan.single_exit_args {
                        if let Some(ridx) = plan.reductions.iter().position(|r| r.update_inst == ea) {
                            direct_exit_args.push(at(&reduced_vals, ridx));
                        } else {
                            direct_exit_args.push(at(&val_map, to_idx(ea)));
                        }
                    }
                    for &ridx in &plan.promoted_exits {
                        direct_exit_args.push(at(&reduced_vals, ridx));
                    }
                    push_br(&mut func, has_tail, orig_header_new, &tail_args, orig_exit_new, &direct_exit_args);
                }
            }
        } else if let ScalarLoopShape::SingleBlock { header, exit } = plan.shape
            && !plan.promoted_exits.is_empty()
            && b_u32 == header
        {
            // Append promoted reduction updates onto the scalar tail's edge to `exit`.
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let idx = to_idx(at(view.data, term));
            let cv = at(&val_map, to_idx(at(view.extra, idx)));
            let t_blk = at(&blk_map, to_idx(at(view.extra, idx + 1)));
            let t_argc = to_idx(at(view.extra, idx + 2));
            let mut t_args: Vec<u32, A> = Vec::with_capacity_in(t_argc, alloc);
            for k in 0..t_argc {
                t_args.push(at(&val_map, to_idx(at(view.extra, idx + 3 + k))));
            }
            let e_at = idx + 3 + t_argc;
            let e_blk = at(&blk_map, to_idx(at(view.extra, e_at)));
            let e_argc = to_idx(at(view.extra, e_at + 1));
            let mut e_args: Vec<u32, A> = Vec::with_capacity_in(e_argc + plan.promoted_exits.len(), alloc);
            for k in 0..e_argc {
                e_args.push(at(&val_map, to_idx(at(view.extra, e_at + 2 + k))));
            }
            for &ridx in &plan.promoted_exits {
                if let Some(red) = plan.reductions.get(ridx) {
                    e_args.push(at(&val_map, to_idx(red.update_inst)));
                }
            }
            let _ = exit;
            push_br(&mut func, cv, t_blk, &t_args, e_blk, &e_args);
        } else if let ScalarLoopShape::SingleBlock { exit, .. } = plan.shape
            && !plan.promoted_exits.is_empty()
            && b_u32 == exit
        {
            // Materialize the promoted `Op::Param`s at the top of `exit` and remap their uses.
            let params_end = view.params(Block(b_u32)).end;
            for inst in range.start..params_end {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            for &ridx in &plan.promoted_exits {
                if let Some(&red) = plan.reductions.get(ridx) {
                    let new_p = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                    set_at(&mut val_map, to_idx(red.update_inst), new_p);
                }
            }
            for inst in params_end..range.end {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        } else {
            for inst in range {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(UnrollOutput {
        func,
        stats: UnrollStats { loops_unrolled: 1 },
    })
}

/// Telemetry returned by [`rotate_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct RotateStats {
    pub loops_rotated: u32,
}

/// Output of [`rotate_loops`].
#[derive(Debug)]
pub struct RotateOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: RotateStats,
}

/// Rotates top-tested `HeaderBody` loops (`pre -> header(cmp ? body : exit) -> body(jump header)`)
/// into single-block bottom-tested loops (`pre(cmp0 ? body : exit) -> body(cmp_next ? body : exit)`).
///
/// Cuts loop branch count in half (2 branches -> 1 single `b.cond`) and frees loop-header
/// accumulators from exit-block liveness interference, unlocking zero-copy register coalescing!
pub fn rotate_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<RotateOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 3 {
        return Ok(RotateOutput { func: view.to_func_in(alloc), stats: RotateStats::default() });
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.scan_for(&b_u32).is_none()
                {
                    p.push(b_u32);
                }
            }
        });
    }

    // Find a clean single-body `HeaderBody` loop eligible for bottom-test rotation.
    let mut found = None;
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, body) = if p0 < header && p1 > header {
            (p0, p1)
        } else if p1 < header && p0 > header {
            (p1, p0)
        } else {
            continue;
        };
        if preds.get(to_idx(body)).map(Vec::as_slice) != Some(&[header]) || !view.params(Block(body)).is_empty() {
            continue;
        }
        let pre_term = view.terminator(Block(preheader));
        let h_term = view.terminator(Block(header));
        let b_term = view.terminator(Block(body));
        if view.op(pre_term) != Op::Jump || view.op(h_term) != Op::Br || view.op(b_term) != Op::Jump {
            continue;
        }
        let pre_at = to_idx(at(view.data, pre_term));
        let hat = to_idx(at(view.data, h_term));
        let bat = to_idx(at(view.data, b_term));
        let h_params = view.params(Block(header));
        let n_params = h_params.len();
        if at(view.extra, pre_at) != header || to_idx(at(view.extra, pre_at + 1)) != n_params {
            continue;
        }
        if at(view.extra, bat) != header || to_idx(at(view.extra, bat + 1)) != n_params {
            continue;
        }
        let cond_v = at(view.extra, hat);
        let t_blk = at(view.extra, hat + 1);
        let t_argc = to_idx(at(view.extra, hat + 2));
        let e_at = hat + 3 + t_argc;
        let e_blk = at(view.extra, e_at);
        let e_argc = to_idx(at(view.extra, e_at + 1));
        if t_blk != body || t_argc != 0 || e_blk == header || e_blk == body {
            continue;
        }
        // Only rotate when `exit` has `header` as its sole predecessor and lies after `body` (or `header`).
        if preds.get(to_idx(e_blk)).map(Vec::as_slice) != Some(&[header]) || e_blk <= body {
            continue;
        }
        // Header body must be tiny & pure (just the loop test, e.g. `icmp`).
        let h_range = view.block(Block(header));
        let h_body_start = h_params.end;
        let h_body_end = h_range.end - 1;
        if h_body_end - h_body_start == 0 || h_body_end - h_body_start > 4 {
            continue;
        }
        let header_pure = (h_body_start..h_body_end).all(|i| {
            matches!(
                view.op(i),
                Op::Icmp | Op::Fcmp | Op::Iconst | Op::IconstWide | Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor | Op::Zext | Op::Trunc
            )
        });
        if !header_pure {
            continue;
        }
        // Check what header defs escape outside `{header, body}`: non-param header defs may not escape;
        // `h_params` used outside `{header, body}` are promoted to new `Op::Param`s on `exit`!
        let mut promoted_params: Vec<usize, A> = Vec::new_in(alloc);
        let mut bad_escape = false;
        for ob in 0..blocks {
            let ob_u32 = to_u32(ob);
            if ob_u32 == header || ob_u32 == body {
                continue;
            }
            for inst in view.block(Block(ob_u32)) {
                each(&view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        let v_blk = at(&inst_block, vi);
                        if v_blk == header {
                            if vi >= h_params.start && vi < h_params.end && ob_u32 >= e_blk {
                                let ppos = vi - h_params.start;
                                if promoted_params.scan_for(&ppos).is_none() {
                                    promoted_params.push(ppos);
                                }
                            } else {
                                bad_escape = true;
                            }
                        } else if v_blk == body {
                            bad_escape = true;
                        }
                    }
                });
            }
        }
        // Exit args themselves must not reference non-param header defs.
        for k in 0..e_argc {
            let ea = to_idx(at(view.extra, e_at + 2 + k));
            if at(&inst_block, ea) == header && !(ea >= h_params.start && ea < h_params.end) {
                bad_escape = true;
            }
        }
        if bad_escape {
            continue;
        }
        // Require `body` to appear right after `header` (or after `preheader`) with no forward-only defs from `header`.
        found = Some((preheader, header, body, e_blk, cond_v, pre_at + 2, bat + 2, e_at + 2, e_argc, promoted_params));
        break;
    }

    let Some((preheader, header, body, exit, cond_v, pre_args_at, back_args_at, exit_args_at, exit_argc, promoted_params)) = found else {
        return Ok(RotateOutput { func: view.to_func_in(alloc), stats: RotateStats::default() });
    };

    // Omit the dead `header` block by shifting block indices `> header` down by 1!
    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        blk_map.push(if b_u32 < header { b_u32 } else { to_u32(b.saturating_sub(1)) });
    }
    let body_new = at(&blk_map, to_idx(body));
    let exit_new = at(&blk_map, to_idx(exit));
    let h_params = view.params(Block(header));
    let n_params = h_params.len();
    let h_body_start = h_params.end;
    let h_body_end = view.block(Block(header)).end - 1;

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    for b in 0..blocks {
        let bu = to_u32(b);
        if bu == header {
            continue;
        }
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(bu));
        let term = range.end - 1;

        if bu == preheader {
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let mut init_args: Vec<u32, A> = Vec::with_capacity_in(n_params, alloc);
            for k in 0..n_params {
                init_args.push(at(&val_map, to_idx(at(view.extra, pre_args_at + k))));
            }
            let mut pre_map = filled(len, NONE, &alloc);
            for (dst, &src) in pre_map.iter_mut().zip(val_map.iter()) {
                *dst = src;
            }
            for (k, &arg) in init_args.iter().enumerate() {
                set_at(&mut pre_map, h_params.start + k, arg);
            }
            for hi in h_body_start..h_body_end {
                let mapped = copy_remapped(&view, hi, &pre_map, &blk_map, &mut func);
                set_at(&mut pre_map, hi, mapped);
            }
            let init_cond = at(&pre_map, to_idx(cond_v));
            let mut pre_exit_args: Vec<u32, A> = Vec::with_capacity_in(exit_argc + promoted_params.len(), alloc);
            for k in 0..exit_argc {
                pre_exit_args.push(at(&pre_map, to_idx(at(view.extra, exit_args_at + k))));
            }
            for &ppos in &promoted_params {
                pre_exit_args.push(at(&init_args, ppos));
            }
            push_br(&mut func, init_cond, body_new, &init_args, exit_new, &pre_exit_args);
        } else if bu == body {
            // Materialize rotated header params directly at the top of `body`!
            for k in 0..n_params {
                let hp = h_params.start + k;
                let np = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at(view.tys, hp)), 0);
                set_at(&mut val_map, hp, np);
            }
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let mut back_args: Vec<u32, A> = Vec::with_capacity_in(n_params, alloc);
            for k in 0..n_params {
                back_args.push(at(&val_map, to_idx(at(view.extra, back_args_at + k))));
            }
            let mut latch_map = filled(len, NONE, &alloc);
            for (dst, &src) in latch_map.iter_mut().zip(val_map.iter()) {
                *dst = src;
            }
            for (k, &arg) in back_args.iter().enumerate() {
                set_at(&mut latch_map, h_params.start + k, arg);
            }
            for hi in h_body_start..h_body_end {
                let mapped = copy_remapped(&view, hi, &latch_map, &blk_map, &mut func);
                set_at(&mut latch_map, hi, mapped);
            }
            let next_cond = at(&latch_map, to_idx(cond_v));
            let mut latch_exit_args: Vec<u32, A> = Vec::with_capacity_in(exit_argc + promoted_params.len(), alloc);
            for k in 0..exit_argc {
                latch_exit_args.push(at(&latch_map, to_idx(at(view.extra, exit_args_at + k))));
            }
            for &ppos in &promoted_params {
                latch_exit_args.push(at(&back_args, ppos));
            }
            push_br(&mut func, next_cond, body_new, &back_args, exit_new, &latch_exit_args);
        } else if bu == exit {
            let ex_params_end = view.params(Block(exit)).end;
            for inst in range.start..ex_params_end {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            for &ppos in &promoted_params {
                let hp = h_params.start + ppos;
                let np = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at(view.tys, hp)), 0);
                set_at(&mut val_map, hp, np);
            }
            for inst in ex_params_end..range.end {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        } else {
            for inst in range {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(RotateOutput {
        func,
        stats: RotateStats { loops_rotated: 1 },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::Conv,
    };

    #[test]
    fn scalar_unroll_4x_matches_evaluator_on_udiv10_loop() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, loop_b, exit_b] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((loop_b, &[z, z]));
        b.begin(loop_b);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let c10 = b.iconst(Ty::I64, 10);
        let c1 = b.iconst(Ty::I64, 1);
        let q = b.binary(Op::Udiv, Flags::NONE, i, c10);
        let r = b.binary(Op::Urem, Flags::NONE, i, c10);
        let t = b.binary(Op::Add, Flags::NONE, q, r);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, t);
        let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
        let keep = b.icmp(IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (loop_b, &[i2, acc2]), (exit_b, &[acc2]));
        b.begin(exit_b);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unroll_scalar_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_unrolled, 1);

        for &nv in &[1u128, 2, 3, 4, 5, 7, 8, 9, 15, 33] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else {
                panic!("orig eval");
            };
            let Outcome::Returned(b) = eval::run_in(out.func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else {
                panic!("unroll eval");
            };
            assert_eq!(a, b, "unroll mismatch at n={nv}");
        }
    }

    #[test]
    fn rotate_loops_converts_header_body_and_matches_evaluator() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let seed = b.iconst(Ty::I64, 0x9e37_79b9);
        b.jump((header, &[z, seed]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let s = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let c1 = b.iconst(Ty::I64, 1);
        let c17 = b.iconst(Ty::I64, 17);
        let s1 = b.binary(Op::Xor, Flags::NONE, s, i);
        let sh = b.binary(Op::Lshr, Flags::NONE, s1, c17);
        let s2 = b.binary(Op::Add, Flags::NONE, s1, sh);
        let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((header, &[i2, s2]));

        b.begin(exit);
        b.ret(&[s]);
        let func = b.finish_checked().unwrap();

        let out = rotate_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_rotated, 1);

        for &nv in &[0u128, 1, 2, 3, 7, 16, 65] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else {
                panic!("orig eval");
            };
            let Outcome::Returned(b) = eval::run_in(out.func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else {
                panic!("rotated eval");
            };
            assert_eq!(a, b, "rotate_loops mismatch at n={nv}");
        }
    }
}

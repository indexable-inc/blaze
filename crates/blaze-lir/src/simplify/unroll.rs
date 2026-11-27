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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RedKind {
    /// Associative integer reduction (`Add`, `Xor`, `Or`, `And`): 4 independent accumulators.
    AssocInt,
    /// Strict IEEE-754 FP reduction (`Fadd` on `F64`/`F32`): 4 body evals first, then 4 chained `fadd`s.
    StrictFadd,
    /// Non-associative serial recurrence (`s -> f(s, iter)`): `update_inst` stays in `body_insts`,
    /// threaded `s0 -> s1 -> s2 -> s3 -> s4` across the 4 unrolled bodies while batching unit loads.
    Serial,
}

#[derive(Clone, Copy)]
struct ScalarReduction {
    param_pos: usize,
    param_inst: u32,
    init_val: u32,
    op: Op,
    flags: Flags,
    ty: Ty,
    kind: RedKind,
    update_inst: u32,
    elem_val: u32,
}

impl ScalarReduction {
    #[inline]
    fn is_single_acc(self) -> bool {
        matches!(self.kind, RedKind::StrictFadd | RedKind::Serial)
    }
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
    /// Unit-stride scalar loads (`i8`/`i16`/`i32`/`i64`/`ptr`/`f32`/`f64`) emitted contiguously
    /// (`+0, +stride, +2*stride, +3*stride`) for `ldp` pair fusion!
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
        // or trapping variable divisions! Non-volatile scalar `Load`s are allowed (vectorizable
        // loops have already been vectorized earlier in the pipeline, leaving their scalar tails
        // with 3 header predecessors).
        let mut body_ok = true;
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

        // Match every non-IV header parameter as:
        // 1. An associative integer reduction (`Add`, `Xor`, `Or`, `And` -> 4 independent accumulators),
        // 2. A strict-order IEEE-754 FP reduction (`Fadd` on `F64`/`F32` -> 1 sequentially threaded accumulator), OR
        // 3. A general non-associative serial recurrence (`s -> f(s, iter)` -> threaded sequentially across 4
        //    unrolled bodies while batching 4 contiguous unit-stride loads into `ldp` pairs!).
        let iv_next_idx = to_idx(iv_next_inst);
        let mut reductions: Vec<ScalarReduction, A> = Vec::new_in(alloc);
        let mut is_red_upd = filled(len, false, &alloc);
        let mut bad_param = false;
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
            if at(&inst_block, bi) != body_block || bi == iv_next_idx || bi == ci {
                bad_param = true;
                break;
            }
            let is_int_red = (p_ty == Ty::I64 || p_ty == Ty::I32)
                && matches!(bop, Op::Add | Op::Xor | Op::Or | Op::And);
            let is_fp_red = (p_ty == Ty::F64 || p_ty == Ty::F32) && bop == Op::Fadd;
            let mut matched_simple = None;
            if is_int_red || is_fp_red {
                let bat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
                let elem_opt = if l == p_inst {
                    Some(r)
                } else if r == p_inst {
                    Some(l)
                } else {
                    None
                };
                if let Some(elem_val) = elem_opt {
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
                    if !other_use {
                        matched_simple = Some((
                            if is_fp_red { RedKind::StrictFadd } else { RedKind::AssocInt },
                            elem_val,
                        ));
                    }
                }
            }
            if let Some((kind, elem_val)) = matched_simple {
                set_at(&mut is_red_upd, bi, true);
                reductions.push(ScalarReduction {
                    param_pos: pos,
                    param_inst: p_inst,
                    init_val: at(&pre_args, pos),
                    op: bop,
                    flags: bflags,
                    ty: p_ty,
                    kind,
                    update_inst: back_v,
                    elem_val,
                });
            } else if matches!(p_ty, Ty::I64 | Ty::I32 | Ty::F64 | Ty::F32) {
                reductions.push(ScalarReduction {
                    param_pos: pos,
                    param_inst: p_inst,
                    init_val: at(&pre_args, pos),
                    op: bop,
                    flags: bflags,
                    ty: p_ty,
                    kind: RedKind::Serial,
                    update_inst: back_v,
                    elem_val: NONE,
                });
            } else {
                bad_param = true;
                break;
            }
        }
        if bad_param || reductions.is_empty() {
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
                is_loop_blk(at(&inst_block, ei))
                    && !at(&is_red_upd, ei)
                    && !reductions.iter().any(|r| r.update_inst == ea)
            })
        {
            continue;
        }

        // Identify unit-stride scalar loads (`i8`/`i16`/`i32`/`i64`/`ptr`/`f32`/`f64`) addressed as
        // `ptr_add(base, iv << shift)` / `ptr_add(base, iv * stride)` (or `ptr_add(base, iv)` for `i8`)
        // so we emit all 4 adjacent loads per base contiguously (`+0, +stride, +2*stride, +3*stride`)
        // for AArch64 `ldp x, x` / `ldp w, w` / `ldp d, d` pair fusion!
        let mut unit_loads: Vec<UnitLoad, A> = Vec::new_in(alloc);
        let mut is_unit_load_or_addr = filled(len, false, &alloc);
        let mut is_unit_addr_helper = filled(len, false, &alloc);
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
        let is_iv_val = |v: u32| -> bool {
            v == iv_inst
                || (view.op(to_idx(v)) == Op::Zext
                    && at(&inst_block, to_idx(v)) == body_block
                    && at(view.data, to_idx(v)) == iv_inst)
        };
        for inst in body_start..body_end {
            if view.op(inst) == Op::Load {
                let lty = Ty(at(view.tys, inst));
                let shift = if lty == Ty::F64 || lty == Ty::I64 || lty == Ty::PTR {
                    3u32
                } else if lty == Ty::F32 || lty == Ty::I32 {
                    2
                } else if lty == Ty::I16 {
                    1
                } else if lty == Ty::I8 {
                    0
                } else {
                    continue;
                };
                let stride = 1i32 << shift;
                let idx = to_idx(at(view.data, inst));
                let ptr = to_idx(at(view.extra, idx));
                let off = at(view.extra, idx + 1).cast_signed();
                if off.checked_add(3 * stride).is_none() {
                    continue;
                }
                if view.op(ptr) == Op::PtrAdd && at(&inst_block, ptr) == body_block {
                    let pat = to_idx(at(view.data, ptr));
                    let base = at(view.extra, pat);
                    let by_u32 = at(view.extra, pat + 1);
                    let by = to_idx(by_u32);
                    if !is_loop_blk(at(&inst_block, to_idx(base))) {
                        let (is_unit, extra_zext) = if shift == 0 {
                            let z = if by_u32 != iv_inst && is_iv_val(by_u32) { Some(by) } else { None };
                            (is_iv_val(by_u32), z)
                        } else if at(&inst_block, by) == body_block {
                            match view.op(by) {
                                Op::Shl => {
                                    let sat = to_idx(at(view.data, by));
                                    let lhs = at(view.extra, sat);
                                    let ok = is_iv_val(lhs)
                                        && const_u64(view, to_idx(at(view.extra, sat + 1))) == Some(u64::from(shift));
                                    let z = if ok && lhs != iv_inst { Some(to_idx(lhs)) } else { None };
                                    (ok, z)
                                }
                                Op::Mul => {
                                    let mat = to_idx(at(view.data, by));
                                    let (l, r) = (at(view.extra, mat), at(view.extra, mat + 1));
                                    if is_iv_val(l) && const_u64(view, to_idx(r)) == Some(1u64 << shift) {
                                        (true, (l != iv_inst).then_some(to_idx(l)))
                                    } else if is_iv_val(r) && const_u64(view, to_idx(l)) == Some(1u64 << shift) {
                                        (true, (r != iv_inst).then_some(to_idx(r)))
                                    } else {
                                        (false, None)
                                    }
                                }
                                _ => (false, None),
                            }
                        } else {
                            (false, None)
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
                            if shift > 0 && at(&inst_block, by) == body_block {
                                set_at(&mut is_unit_addr_helper, by, true);
                            }
                            if let Some(zi) = extra_zext {
                                set_at(&mut is_unit_addr_helper, zi, true);
                            }
                        }
                    }
                }
            }
        }
        // Only unroll non-associative serial recurrences when there is a single recurrence with
        // unit-stride loads (so 4x unrolling batches 4 contiguous loads into 2 `ldp` pairs and cuts
        // branch/IV overhead 4x, e.g. `wyhash_mix`).
        let has_serial = reductions.iter().any(|r| r.kind == RedKind::Serial);
        if has_serial
            && (reductions.len() != 1
                || unit_loads.is_empty()
                || reductions.iter().any(|r| at(&is_unit_load_or_addr, to_idx(r.update_inst))))
        {
            continue;
        }
        // Drop absorbed unit-load address helpers (`shl(iv, shift)` / `mul(iv, stride)` / `zext(iv)`)
        // when ALL of their body uses were absorbed `ptr_add`s or helpers.
        for _ in 0..2 {
            for inst in body_start..body_end {
                if !at(&is_unit_load_or_addr, inst) && at(&is_unit_addr_helper, inst) {
                    let mut any_kept_use = false;
                    for u_inst in body_start..body_end {
                        if at(&is_unit_load_or_addr, u_inst) || u_inst == iv_next_idx || u_inst == ci {
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
                    if !any_kept_use {
                        set_at(&mut is_unit_load_or_addr, inst, true);
                    }
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

            // --- `u_pre`: compute `u_end = iv_init + (rem & !3)`, running pointers, and init accumulators ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_starts_zero = const_u64(&view, to_idx(plan.iv_init)) == Some(0);
            let k_mask = push_iconst(&mut func, plan.iv_ty, !(unroll_u64 - 1));
            let u_cnt = push_bin(&mut func, Op::And, plan.iv_ty, rem_val, k_mask);
            let u_end = if iv_starts_zero {
                u_cnt
            } else {
                push_bin(&mut func, Op::Add, plan.iv_ty, iv_init_new, u_cnt)
            };

            let mut needs_iv_u = plan.reductions.iter().any(|r| r.kind != RedKind::Serial && r.elem_val == plan.iv_inst);
            if !needs_iv_u {
                for &bi in &plan.body_insts {
                    each(&view, to_idx(bi), |item| {
                        if let Item::Value(v) = item
                            && v == plan.iv_inst
                        {
                            needs_iv_u = true;
                        }
                    });
                }
            }
            let use_ptr_iv = !plan.unit_loads.is_empty() && !needs_iv_u;

            #[derive(Clone, Copy)]
            struct UnrollPtrStream {
                base: u32,
                shift: u32,
                init_ptr: u32,
            }
            let mut ptr_streams: Vec<UnrollPtrStream, A> = Vec::with_capacity_in(plan.unit_loads.len(), alloc);
            let mut load_stream_idx: Vec<usize, A> = Vec::with_capacity_in(plan.unit_loads.len(), alloc);
            let mut p0_end = NONE;
            if use_ptr_iv {
                let iv_init64 = if iv_starts_zero {
                    NONE
                } else if plan.iv_ty == Ty::I32 {
                    push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv_init_new)
                } else {
                    iv_init_new
                };
                for ul in &plan.unit_loads {
                    if let Some(si) = ptr_streams.iter().position(|s| s.base == ul.base && s.shift == ul.shift) {
                        load_stream_idx.push(si);
                    } else {
                        let base_new = at(&val_map, to_idx(ul.base));
                        let init_ptr = if iv_starts_zero {
                            base_new
                        } else {
                            let init_off = if ul.shift == 0 {
                                iv_init64
                            } else {
                                let ks = push_iconst(&mut func, Ty::I64, u64::from(ul.shift));
                                push_bin(&mut func, Op::Shl, Ty::I64, iv_init64, ks)
                            };
                            push_bin(&mut func, Op::PtrAdd, Ty::PTR, base_new, init_off)
                        };
                        let si = ptr_streams.len();
                        ptr_streams.push(UnrollPtrStream { base: ul.base, shift: ul.shift, init_ptr });
                        load_stream_idx.push(si);
                    }
                }
                if let Some(s0) = ptr_streams.first() {
                    let u_cnt64 = if plan.iv_ty == Ty::I32 {
                        push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, u_cnt)
                    } else {
                        u_cnt
                    };
                    let total_bytes = if s0.shift == 0 {
                        u_cnt64
                    } else {
                        let ks = push_iconst(&mut func, Ty::I64, u64::from(s0.shift));
                        push_bin(&mut func, Op::Shl, Ty::I64, u_cnt64, ks)
                    };
                    p0_end = push_bin(&mut func, Op::PtrAdd, Ty::PTR, s0.init_ptr, total_bytes);
                }
            }

            let n_iv_params = if use_ptr_iv { ptr_streams.len() } else { 1 };
            let mut init_args: Vec<u32, A> =
                Vec::with_capacity_in(n_iv_params + plan.reductions.len() * UNROLL_FACTOR, alloc);
            if use_ptr_iv {
                for s in &ptr_streams {
                    init_args.push(s.init_ptr);
                }
            } else {
                init_args.push(iv_init_new);
            }
            for red in &plan.reductions {
                init_args.push(at(&val_map, to_idx(red.init_val)));
                if !red.is_single_acc() {
                    let id_val = if red.op == Op::And { u64::MAX } else { 0 };
                    let id_c = push_iconst(&mut func, red.ty, id_val);
                    for _ in 1..UNROLL_FACTOR {
                        init_args.push(id_c);
                    }
                }
            }
            push_jump(&mut func, u_loop_blk, &init_args);

            // --- `u_loop`: 4x unrolled scalar loop with pointer-IV + breadth-first ILP! ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut ptr_params: Vec<u32, A> = Vec::with_capacity_in(ptr_streams.len(), alloc);
            let iv0 = if use_ptr_iv {
                for _ in 0..ptr_streams.len() {
                    ptr_params.push(push_inst(&mut func, Op::Param, Flags::NONE, Ty::PTR, 0));
                }
                NONE
            } else {
                push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0)
            };
            let mut acc_params: Vec<[u32; UNROLL_FACTOR], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                let a0 = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0);
                if red.is_single_acc() {
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

            // Emit all 4 contiguous loads per `UnitLoad` stream (`+0, +stride, +2*stride, +3*stride`)
            // so AArch64 `isel.rs` fuses them into post-index `ldp x, x, [p], #16` pairs!
            let mut ptr_nexts = filled(ptr_streams.len(), NONE, &alloc);
            if use_ptr_iv {
                for (lidx, ul) in plan.unit_loads.iter().enumerate() {
                    let si = at(&load_stream_idx, lidx);
                    let base_p = at(&ptr_params, si);
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
                    let is_last_for_stream =
                        !load_stream_idx.get(lidx + 1..).is_some_and(|rest| rest.iter().any(|&s| s == si));
                    if is_last_for_stream {
                        let step_bytes = unroll_u64 << ul.shift;
                        let k_sb = push_iconst(&mut func, Ty::I64, step_bytes);
                        let p_next = push_bin(&mut func, Op::PtrAdd, Ty::PTR, base_p, k_sb);
                        set_at(&mut ptr_nexts, si, p_next);
                    }
                }
            } else if !plan.unit_loads.is_empty() {
                let iv0_64 = if plan.iv_ty == Ty::I32 {
                    push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv0)
                } else {
                    iv0
                };
                let mut boff_by_shift: [(u32, u32); 4] = [(u32::MAX, NONE); 4];
                for ul in &plan.unit_loads {
                    let boff = if ul.shift == 0 {
                        iv0_64
                    } else if let Some(&(_, existing)) = boff_by_shift.iter().find(|&&(s, _)| s == ul.shift) {
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

            // Emit replicated body expressions:
            // - For serial recurrences (`wyhash_mix`), thread `cur_serial` lane-by-lane across `0..4`.
            // - For non-serial reductions (`arx_mix`, `div_const_sum`, FP dot), emit **2x2 pair
            //   interleaving** — pair `(0, 1)` breadth-first through `body_insts` + reduction updates,
            //   then pair `(2, 3)` breadth-first through `body_insts` + reduction updates — saturating
            //   2x 64-bit MUL pipes while dropping peak live GPRs by 2 for a frameless leaf!
            let has_serial = plan.reductions.iter().any(|r| r.kind == RedKind::Serial);
            if has_serial {
                let mut cur_serial: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for ridx in 0..plan.reductions.len() {
                    cur_serial.push(acc_params.get(ridx).map_or(NONE, |row| row[0]));
                }
                for (u, lm) in local_maps.iter_mut().enumerate() {
                    if !use_ptr_iv {
                        let iv_u = if u == 0 {
                            iv0
                        } else if needs_iv_u {
                            let ku = push_iconst(&mut func, plan.iv_ty, u64::try_from(u).unwrap_or(0));
                            push_bin(&mut func, Op::Add, plan.iv_ty, iv0, ku)
                        } else {
                            iv0
                        };
                        set_at(lm, to_idx(plan.iv_inst), iv_u);
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        let acc_u = if red.kind == RedKind::Serial {
                            at(&cur_serial, ridx)
                        } else {
                            acc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE)
                        };
                        set_at(lm, to_idx(red.param_inst), acc_u);
                    }
                    for &bi in &plan.body_insts {
                        let bi_idx = to_idx(bi);
                        let mapped = copy_remapped(&view, bi_idx, lm, &blk_map, &mut func);
                        set_at(lm, bi_idx, mapped);
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        if red.kind == RedKind::Serial {
                            let next_s = at(lm, to_idx(red.update_inst));
                            set_at(&mut cur_serial, ridx, next_s);
                            if u + 1 == UNROLL_FACTOR
                                && let Some(row) = u_next_accs.get_mut(ridx)
                            {
                                row[0] = next_s;
                            }
                        }
                    }
                }
            } else {
                let mut cur_fadd: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for ridx in 0..plan.reductions.len() {
                    cur_fadd.push(acc_params.get(ridx).map_or(NONE, |row| row[0]));
                }
                for pair in [[0usize, 1usize], [2usize, 3usize]] {
                    for u in pair {
                        if let Some(lm) = local_maps.get_mut(u) {
                            if !use_ptr_iv {
                                let iv_u = if u == 0 {
                                    iv0
                                } else if needs_iv_u {
                                    let ku = push_iconst(&mut func, plan.iv_ty, u64::try_from(u).unwrap_or(0));
                                    push_bin(&mut func, Op::Add, plan.iv_ty, iv0, ku)
                                } else {
                                    iv0
                                };
                                set_at(lm, to_idx(plan.iv_inst), iv_u);
                            }
                            for (ridx, red) in plan.reductions.iter().enumerate() {
                                let acc_u = acc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                                set_at(lm, to_idx(red.param_inst), acc_u);
                            }
                        }
                    }
                    for &bi in &plan.body_insts {
                        let bi_idx = to_idx(bi);
                        for u in pair {
                            if let Some(lm) = local_maps.get_mut(u) {
                                let mapped = copy_remapped(&view, bi_idx, lm, &blk_map, &mut func);
                                set_at(lm, bi_idx, mapped);
                            }
                        }
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        match red.kind {
                            RedKind::StrictFadd => {
                                let mut cur = at(&cur_fadd, ridx);
                                for u in pair {
                                    if let Some(lm) = local_maps.get(u) {
                                        let elem_v = at(lm, to_idx(red.elem_val));
                                        let idx = to_u32(func.extra.len());
                                        func.extra.push(cur);
                                        func.extra.push(elem_v);
                                        cur = push_inst(&mut func, red.op, red.flags, red.ty, idx);
                                    }
                                }
                                set_at(&mut cur_fadd, ridx, cur);
                                if let Some(row) = u_next_accs.get_mut(ridx) {
                                    row[0] = cur;
                                }
                            }
                            RedKind::AssocInt => {
                                for u in pair {
                                    if let Some(lm) = local_maps.get(u) {
                                        let elem_v = at(lm, to_idx(red.elem_val));
                                        let acc_u = acc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                                        let next_v = push_bin(&mut func, red.op, red.ty, acc_u, elem_v);
                                        if let Some(row) = u_next_accs.get_mut(ridx) {
                                            set_at(row, u, next_v);
                                        }
                                    }
                                }
                            }
                            RedKind::Serial => {}
                        }
                    }
                }
            }

            let mut back_args: Vec<u32, A> =
                Vec::with_capacity_in(n_iv_params + plan.reductions.len() * UNROLL_FACTOR, alloc);
            let mut red_args: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len() * UNROLL_FACTOR, alloc);
            let iv_next = if use_ptr_iv {
                for &pn in &ptr_nexts {
                    back_args.push(pn);
                }
                NONE
            } else {
                let k_step4 = push_iconst(&mut func, plan.iv_ty, unroll_u64);
                let next = push_bin(&mut func, Op::Add, plan.iv_ty, iv0, k_step4);
                back_args.push(next);
                next
            };
            for (ridx, red) in plan.reductions.iter().enumerate() {
                if let Some(row) = u_next_accs.get(ridx) {
                    if red.is_single_acc() {
                        back_args.push(row[0]);
                        red_args.push(row[0]);
                    } else {
                        for &v in row {
                            back_args.push(v);
                            red_args.push(v);
                        }
                    }
                }
            }
            let keep = if use_ptr_iv {
                push_icmp(&mut func, IntCond::Ult, at(&ptr_nexts, 0), p0_end)
            } else {
                push_icmp(&mut func, IntCond::Ult, iv_next, u_end)
            };
            push_br(&mut func, keep, u_loop_blk, &back_args, u_red_blk, &red_args);

            // --- `u_red`: tree-reduce associative accumulators (uses `u_end` from `u_pre`!) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut reduced_vals: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                if red.is_single_acc() {
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
                    tail_args.push(u_end);
                } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                    tail_args.push(at(&reduced_vals, ridx));
                } else {
                    tail_args.push(at(&val_map, to_idx(pre_arg)));
                }
            }
            match plan.shape {
                ScalarLoopShape::HeaderBody { .. } => push_jump(&mut func, orig_header_new, &tail_args),
                ScalarLoopShape::SingleBlock { .. } => {
                    let has_tail = push_icmp(&mut func, plan.cmp_cond, u_end, bound_new);
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

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
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

/// Rotates top-tested natural loops (both single-block bodies and multi-block / nested outer
/// loops with 1..=4 latches) whose `header` is a pure guard (`params` + pure `icmp`/`br` ->
/// `body_entry` | `exit`, with `preheader` ending in `Jump(header, init_args)` or
/// `Br(cond, header(init_args), other)` and each latch ending in `Jump(header, next_args)`)
/// into bottom-tested loops.
///
/// Eliminates the unconditional back-edge `b header` on EVERY iteration (both inner and outer
/// loops like `bsearch_batch`'s unswitched `h_q` & `h_bs`) and frees loop-header accumulators
/// from exit-block liveness interference, unlocking zero-copy register coalescing!
pub fn rotate_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<RotateOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut total_rotated = 0u32;
    const MAX_ROTATIONS: usize = 16;
    for _ in 0..MAX_ROTATIONS {
        let Some(next) = rotate_one_loop(cur.view(), alloc)? else {
            break;
        };
        cur = next;
        total_rotated += 1;
    }
    Ok(RotateOutput {
        func: cur,
        stats: RotateStats { loops_rotated: total_rotated },
    })
}

fn rotate_one_loop<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 3 {
        return Ok(None);
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

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PreEdge {
        Jump,
        BrThen,
        BrElse,
    }

    struct RotatePlan<A: Allocator> {
        preheader: u32,
        pre_edge: PreEdge,
        header: u32,
        body_entry: u32,
        exit: u32,
        body_on_then: bool,
        cond_v: u32,
        pre_args_at: usize,
        latches: Vec<(u32, usize), A>,
        exit_args_at: usize,
        exit_argc: usize,
        promoted_params: Vec<usize, A>,
    }

    // Find any reducible natural loop whose header is a pure guard (`params` + pure `icmp`/`br`).
    let mut found: Option<RotatePlan<A>> = None;
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let Some(h_preds) = preds.get(h) else {
            continue;
        };
        let mut preheader: Option<u32> = None;
        let mut multi_entry = false;
        let mut has_self_loop = false;
        let mut latch_blks: Vec<u32, A> = Vec::new_in(alloc);
        for &p in h_preds {
            if p < header {
                if preheader.is_some() {
                    multi_entry = true;
                    break;
                }
                preheader = Some(p);
            } else if p > header {
                latch_blks.push(p);
            } else {
                has_self_loop = true;
                break;
            }
        }
        let Some(preheader) = preheader else {
            continue;
        };
        if multi_entry || has_self_loop || latch_blks.is_empty() || latch_blks.len() > 4 {
            continue;
        }

        let pre_term = view.terminator(Block(preheader));
        let h_term = view.terminator(Block(header));
        if view.op(h_term) != Op::Br {
            continue;
        }
        let h_params = view.params(Block(header));
        let n_params = h_params.len();
        let (pre_edge, pre_args_at) = match view.op(pre_term) {
            Op::Jump => {
                let pre_at = to_idx(at(view.data, pre_term));
                if at(view.extra, pre_at) != header || to_idx(at(view.extra, pre_at + 1)) != n_params {
                    continue;
                }
                (PreEdge::Jump, pre_at + 2)
            }
            Op::Br => {
                let pidx = to_idx(at(view.data, pre_term));
                let t_blk = at(view.extra, pidx + 1);
                let t_argc = to_idx(at(view.extra, pidx + 2));
                let e_at = pidx + 3 + t_argc;
                let e_blk = at(view.extra, e_at);
                let e_argc = to_idx(at(view.extra, e_at + 1));
                if t_blk == header && e_blk != header && t_argc == n_params {
                    (PreEdge::BrThen, pidx + 3)
                } else if e_blk == header && t_blk != header && e_argc == n_params {
                    (PreEdge::BrElse, e_at + 2)
                } else {
                    continue;
                }
            }
            _ => continue,
        };

        let mut latches: Vec<(u32, usize), A> = Vec::with_capacity_in(latch_blks.len(), alloc);
        let mut latches_ok = true;
        for &lb in &latch_blks {
            let l_term = view.terminator(Block(lb));
            if view.op(l_term) != Op::Jump {
                latches_ok = false;
                break;
            }
            let lat = to_idx(at(view.data, l_term));
            if at(view.extra, lat) != header || to_idx(at(view.extra, lat + 1)) != n_params {
                latches_ok = false;
                break;
            }
            latches.push((lb, lat + 2));
        }
        if !latches_ok {
            continue;
        }

        // Compute natural loop body via backward reachability from `latches` stopping at `header`.
        let mut in_loop = filled(blocks, false, &alloc);
        set_at(&mut in_loop, h, true);
        let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        for &(lb, _) in &latches {
            let li = to_idx(lb);
            if !at(&in_loop, li) {
                set_at(&mut in_loop, li, true);
                work.push(lb);
            }
        }
        while let Some(cur) = work.pop() {
            if let Some(cp) = preds.get(to_idx(cur)) {
                for &p in cp {
                    let pi = to_idx(p);
                    if !at(&in_loop, pi) {
                        set_at(&mut in_loop, pi, true);
                        work.push(p);
                    }
                }
            }
        }
        if at(&in_loop, to_idx(preheader)) {
            continue;
        }

        let hat = to_idx(at(view.data, h_term));
        let cond_v = at(view.extra, hat);
        let t_blk = at(view.extra, hat + 1);
        let t_argc = to_idx(at(view.extra, hat + 2));
        let e_at = hat + 3 + t_argc;
        let e_blk = at(view.extra, e_at);
        let e_argc = to_idx(at(view.extra, e_at + 1));
        if t_blk == header || e_blk == header || t_blk == e_blk {
            continue;
        }

        let (body_entry, exit, body_on_then, exit_args_at, exit_argc) =
            if at(&in_loop, to_idx(t_blk)) && !at(&in_loop, to_idx(e_blk)) && t_argc == 0 {
                (t_blk, e_blk, true, e_at + 2, e_argc)
            } else if at(&in_loop, to_idx(e_blk)) && !at(&in_loop, to_idx(t_blk)) && e_argc == 0 {
                (e_blk, t_blk, false, hat + 3, t_argc)
            } else {
                continue;
            };

        if body_entry < header
            || preds.get(to_idx(body_entry)).map(Vec::as_slice) != Some(&[header])
            || !view.params(Block(body_entry)).is_empty()
        {
            continue;
        }

        // Verify reducible single-entry natural loop in layout order:
        // every loop block `!= header` satisfies `body_entry <= b` and all its preds are in `in_loop`.
        let mut max_loop_blk = header;
        let mut loop_shape_ok = true;
        for b in 0..blocks {
            if b != h && at(&in_loop, b) {
                let bu = to_u32(b);
                if bu < body_entry {
                    loop_shape_ok = false;
                    break;
                }
                if bu > max_loop_blk {
                    max_loop_blk = bu;
                }
                if let Some(bp) = preds.get(b) {
                    if bp.iter().any(|&p| !at(&in_loop, to_idx(p))) {
                        loop_shape_ok = false;
                        break;
                    }
                }
            }
        }
        if !loop_shape_ok {
            continue;
        }

        // Header body must be tiny & pure (just the loop guard, e.g. `icmp`), reading only
        // `h_params` and values defined in blocks `<= preheader`.
        let h_range = view.block(Block(header));
        let h_body_start = h_params.end;
        let h_body_end = h_range.end - 1;
        if h_body_end - h_body_start == 0 || h_body_end - h_body_start > 4 {
            continue;
        }
        let mut header_pure = true;
        for hi in h_body_start..h_body_end {
            if !matches!(
                view.op(hi),
                Op::Icmp
                    | Op::Fcmp
                    | Op::Iconst
                    | Op::IconstWide
                    | Op::Add
                    | Op::Sub
                    | Op::And
                    | Op::Or
                    | Op::Xor
                    | Op::Zext
                    | Op::Trunc
            ) {
                header_pure = false;
                break;
            }
            each(&view, hi, |item| {
                if let Item::Value(v) = item {
                    let vb = at(&inst_block, to_idx(v));
                    if vb != header && vb > preheader {
                        header_pure = false;
                    }
                }
            });
            if !header_pure {
                break;
            }
        }
        let cond_blk = at(&inst_block, to_idx(cond_v));
        if !header_pure || (cond_blk != header && cond_blk > preheader) {
            continue;
        }

        // Verify `pre_args` and each latch's `back_args` are defined in layout order before their edges.
        let mut args_ready = true;
        for k in 0..n_params {
            let pa = at(view.extra, pre_args_at + k);
            if at(&inst_block, to_idx(pa)) > preheader {
                args_ready = false;
                break;
            }
        }
        if args_ready {
            for &(lb, bat) in &latches {
                for k in 0..n_params {
                    let ba = at(view.extra, bat + k);
                    if at(&inst_block, to_idx(ba)) > lb {
                        args_ready = false;
                        break;
                    }
                }
                if !args_ready {
                    break;
                }
            }
        }
        if !args_ready {
            continue;
        }

        // Check value escapes across all blocks:
        // - Non-param header defs may NEVER be used outside `header` (nor in `exit_args`).
        // - Outside `!in_loop` blocks: no body-block def may escape, and any `h_param` used
        //   directly outside the loop is promoted to a new `Op::Param` on `exit`!
        let mut promoted_params: Vec<usize, A> = Vec::new_in(alloc);
        let mut bad_escape = false;
        for ob in 0..blocks {
            let ob_u32 = to_u32(ob);
            if ob_u32 == header {
                continue;
            }
            let ob_in_loop = at(&in_loop, ob);
            for inst in view.block(Block(ob_u32)) {
                each(&view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        let v_blk = at(&inst_block, vi);
                        if v_blk == header {
                            let is_hparam = vi >= h_params.start && vi < h_params.end;
                            if !is_hparam {
                                bad_escape = true;
                            } else if !ob_in_loop {
                                if ob_u32 >= exit {
                                    let ppos = vi - h_params.start;
                                    if promoted_params.scan_for(&ppos).is_none() {
                                        promoted_params.push(ppos);
                                    }
                                } else {
                                    bad_escape = true;
                                }
                            }
                        } else if !ob_in_loop && at(&in_loop, to_idx(v_blk)) {
                            bad_escape = true;
                        }
                    }
                });
            }
        }
        for k in 0..exit_argc {
            let ea = to_idx(at(view.extra, exit_args_at + k));
            let eb = at(&inst_block, ea);
            if eb == header {
                if !(ea >= h_params.start && ea < h_params.end) {
                    bad_escape = true;
                }
            } else if eb > preheader {
                bad_escape = true;
            }
        }
        if bad_escape {
            continue;
        }

        // Promoting `h_params` onto `exit` requires `header -> exit` to be the sole exit edge
        // of `in_loop`, `exit` to have `header` as its sole predecessor, and `exit > max_loop_blk`.
        if !promoted_params.is_empty() {
            if preds.get(to_idx(exit)).map(Vec::as_slice) != Some(&[header]) || exit <= max_loop_blk {
                continue;
            }
            let mut has_other_exit = false;
            for b in 0..blocks {
                if b != h && at(&in_loop, b) {
                    let bterm = view.terminator(Block(to_u32(b)));
                    each(&view, bterm, |item| {
                        if let Item::Edge(edge) = item
                            && !at(&in_loop, to_idx(edge.block))
                        {
                            has_other_exit = true;
                        }
                    });
                }
            }
            if has_other_exit {
                continue;
            }
        }

        found = Some(RotatePlan {
            preheader,
            pre_edge,
            header,
            body_entry,
            exit,
            body_on_then,
            cond_v,
            pre_args_at,
            latches,
            exit_args_at,
            exit_argc,
            promoted_params,
        });
        break;
    }

    let Some(plan) = found else {
        return Ok(None);
    };

    // For `PreEdge::Jump`, omit the dead `header` block (shifting indices `> header` down by 1).
    // For `PreEdge::BrThen`/`PreEdge::BrElse`, reuse `header`'s block index as `rot_guard` so
    // `preheader`'s `Br` edge targets `rot_guard` with 0 args and all other block indices stay fixed!
    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        if plan.pre_edge == PreEdge::Jump {
            blk_map.push(if b_u32 < plan.header { b_u32 } else { to_u32(b.saturating_sub(1)) });
        } else {
            blk_map.push(b_u32);
        }
    }
    let body_new = at(&blk_map, to_idx(plan.body_entry));
    let exit_new = at(&blk_map, to_idx(plan.exit));
    let h_params = view.params(Block(plan.header));
    let n_params = h_params.len();
    let h_body_start = h_params.end;
    let h_body_end = view.block(Block(plan.header)).end - 1;

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

    let emit_init_guard = |func: &mut Func<A>, val_map: &[u32]| {
        let mut init_args: Vec<u32, A> = Vec::with_capacity_in(n_params, alloc);
        for k in 0..n_params {
            init_args.push(at(val_map, to_idx(at(view.extra, plan.pre_args_at + k))));
        }
        let mut pre_map = filled(len, NONE, &alloc);
        for (dst, &src) in pre_map.iter_mut().zip(val_map.iter()) {
            *dst = src;
        }
        for (k, &arg) in init_args.iter().enumerate() {
            set_at(&mut pre_map, h_params.start + k, arg);
        }
        for hi in h_body_start..h_body_end {
            let mapped = copy_remapped(&view, hi, &pre_map, &blk_map, func);
            set_at(&mut pre_map, hi, mapped);
        }
        let init_cond = at(&pre_map, to_idx(plan.cond_v));
        let mut pre_exit_args: Vec<u32, A> =
            Vec::with_capacity_in(plan.exit_argc + plan.promoted_params.len(), alloc);
        for k in 0..plan.exit_argc {
            pre_exit_args.push(at(&pre_map, to_idx(at(view.extra, plan.exit_args_at + k))));
        }
        for &ppos in &plan.promoted_params {
            pre_exit_args.push(at(&init_args, ppos));
        }
        if plan.body_on_then {
            push_br(func, init_cond, body_new, &init_args, exit_new, &pre_exit_args);
        } else {
            push_br(func, init_cond, exit_new, &pre_exit_args, body_new, &init_args);
        }
    };

    let mut val_map = filled(len, NONE, &alloc);
    for b in 0..blocks {
        let bu = to_u32(b);
        if bu == plan.header {
            if plan.pre_edge == PreEdge::Jump {
                continue;
            }
            // Emit split `rot_guard` block in place of `header` (0 params; dominated by `preheader`).
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(view.block_kinds, b));
            emit_init_guard(&mut func, &val_map);
            continue;
        }
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(bu));
        let term = range.end - 1;
        let latch_args_at = plan.latches.iter().find(|&&(lb, _)| lb == bu).map(|&(_, at_idx)| at_idx);

        if bu == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            match plan.pre_edge {
                PreEdge::Jump => emit_init_guard(&mut func, &val_map),
                PreEdge::BrThen => {
                    let pidx = to_idx(at(view.data, term));
                    let p_cond = at(&val_map, to_idx(at(view.extra, pidx)));
                    let t_blk = at(&blk_map, to_idx(at(view.extra, pidx + 1)));
                    let t_argc = to_idx(at(view.extra, pidx + 2));
                    let e_at = pidx + 3 + t_argc;
                    let e_blk = at(&blk_map, to_idx(at(view.extra, e_at)));
                    let e_argc = to_idx(at(view.extra, e_at + 1));
                    let mut e_args: Vec<u32, A> = Vec::with_capacity_in(e_argc, alloc);
                    for k in 0..e_argc {
                        e_args.push(at(&val_map, to_idx(at(view.extra, e_at + 2 + k))));
                    }
                    push_br(&mut func, p_cond, t_blk, &[], e_blk, &e_args);
                }
                PreEdge::BrElse => {
                    let pidx = to_idx(at(view.data, term));
                    let p_cond = at(&val_map, to_idx(at(view.extra, pidx)));
                    let t_blk = at(&blk_map, to_idx(at(view.extra, pidx + 1)));
                    let t_argc = to_idx(at(view.extra, pidx + 2));
                    let mut t_args: Vec<u32, A> = Vec::with_capacity_in(t_argc, alloc);
                    for k in 0..t_argc {
                        t_args.push(at(&val_map, to_idx(at(view.extra, pidx + 3 + k))));
                    }
                    let e_at = pidx + 3 + t_argc;
                    let e_blk = at(&blk_map, to_idx(at(view.extra, e_at)));
                    push_br(&mut func, p_cond, t_blk, &t_args, e_blk, &[]);
                }
            }
        } else if bu == plan.body_entry {
            // Materialize rotated header params directly at the top of `body_entry`!
            for k in 0..n_params {
                let hp = h_params.start + k;
                let np = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at(view.tys, hp)), 0);
                set_at(&mut val_map, hp, np);
            }
            if let Some(back_args_at) = latch_args_at {
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
                let next_cond = at(&latch_map, to_idx(plan.cond_v));
                let mut latch_exit_args: Vec<u32, A> =
                    Vec::with_capacity_in(plan.exit_argc + plan.promoted_params.len(), alloc);
                for k in 0..plan.exit_argc {
                    latch_exit_args.push(at(&latch_map, to_idx(at(view.extra, plan.exit_args_at + k))));
                }
                for &ppos in &plan.promoted_params {
                    latch_exit_args.push(at(&back_args, ppos));
                }
                if plan.body_on_then {
                    push_br(&mut func, next_cond, body_new, &back_args, exit_new, &latch_exit_args);
                } else {
                    push_br(&mut func, next_cond, exit_new, &latch_exit_args, body_new, &back_args);
                }
            } else {
                for inst in range {
                    let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
            }
        } else if let Some(back_args_at) = latch_args_at {
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
            let next_cond = at(&latch_map, to_idx(plan.cond_v));
            let mut latch_exit_args: Vec<u32, A> =
                Vec::with_capacity_in(plan.exit_argc + plan.promoted_params.len(), alloc);
            for k in 0..plan.exit_argc {
                latch_exit_args.push(at(&latch_map, to_idx(at(view.extra, plan.exit_args_at + k))));
            }
            for &ppos in &plan.promoted_params {
                latch_exit_args.push(at(&back_args, ppos));
            }
            if plan.body_on_then {
                push_br(&mut func, next_cond, body_new, &back_args, exit_new, &latch_exit_args);
            } else {
                push_br(&mut func, next_cond, exit_new, &latch_exit_args, body_new, &back_args);
            }
        } else if bu == plan.exit && !plan.promoted_params.is_empty() {
            let ex_params_end = view.params(Block(plan.exit)).end;
            for inst in range.start..ex_params_end {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            for &ppos in &plan.promoted_params {
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

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(Some(func))
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{Conv, SlotKind},
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

    #[test]
    fn scalar_unroll_4x_arx_mix_with_unit_loads_and_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let arr_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let ptr = b.slot_addr(arr_slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(17)));
            b.store(Flags::NONE, v, ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let init = b.iconst(Ty::I64, 0x1234_5678_9abc_def0);
        let golden = b.iconst(Ty::I64, 0x9e37_79b9_7f4a_7c15);
        let c13 = b.iconst(Ty::I64, 13);
        let three = b.iconst(Ty::I64, 3);
        let one = b.iconst(Ty::I64, 1);
        b.jump((header, &[zero, init]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let p = b.ptr_add(Flags::NONE, ptr, off);
        let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let rot = b.binary(Op::Rotl, Flags::NONE, x, c13);
        let m = b.binary(Op::Mul, Flags::NONE, x, golden);
        let mix = b.binary(Op::Xor, Flags::NONE, rot, m);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, mix);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unroll_scalar_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_unrolled, 1);
        assert!(verify(out.func.view()).is_ok());
        // `u_loop` (block 2) must carry `iv0` + 4 independent accumulators (5 params) and 4 contiguous `Load`s!
        let uv = out.func.view();
        assert_eq!(uv.params(Block(2)).len(), 5);
        let u_loads = uv.block(Block(2)).filter(|&inst| uv.op(inst) == Op::Load).count();
        assert_eq!(u_loads, 4);

        for &nv in &[0u128, 1, 2, 3, 4, 5, 7, 8, 9, 13, 16] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv)], 50_000, TestHeap).unwrap() else {
                panic!("orig arx eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &[Val::Int(nv)], 50_000, TestHeap).unwrap() else {
                panic!("unroll arx eval");
            };
            assert_eq!(a, c, "arx_mix unroll mismatch at n={nv}");
        }
    }

    #[test]
    fn scalar_unroll_4x_div_const_sum_with_unit_loads_and_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let arr_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let ptr = b.slot_addr(arr_slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k.wrapping_mul(100_003).wrapping_add(11)));
            b.store(Flags::NONE, v, ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let seven = b.iconst(Ty::I64, 7);
        let three = b.iconst(Ty::I64, 3);
        let one = b.iconst(Ty::I64, 1);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let p = b.ptr_add(Flags::NONE, ptr, off);
        let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let q = b.binary(Op::Udiv, Flags::NONE, v, seven);
        let r = b.binary(Op::Urem, Flags::NONE, v, seven);
        let qr = b.binary(Op::Add, Flags::NONE, q, r);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, qr);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unroll_scalar_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_unrolled, 1);
        assert!(verify(out.func.view()).is_ok());
        let uv = out.func.view();
        assert_eq!(uv.params(Block(2)).len(), 5);
        assert_eq!(uv.block(Block(2)).filter(|&inst| uv.op(inst) == Op::Load).count(), 4);

        for &nv in &[0u128, 1, 2, 3, 4, 5, 7, 8, 9, 13, 16] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv)], 50_000, TestHeap).unwrap() else {
                panic!("orig div_const eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &[Val::Int(nv)], 50_000, TestHeap).unwrap() else {
                panic!("unroll div_const eval");
            };
            assert_eq!(a, c, "div_const_sum unroll mismatch at n={nv}");
        }
    }

    #[test]
    fn scalar_unroll_4x_wyhash_serial_recurrence_and_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let arr_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let ptr = b.slot_addr(arr_slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k.wrapping_mul(0x517c_c1b7_2722_0a95).wrapping_add(3)));
            b.store(Flags::NONE, v, ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let secret = b.iconst(Ty::I64, 0xa076_1d64_78bd_642f);
        let three = b.iconst(Ty::I64, 3);
        let one = b.iconst(Ty::I64, 1);
        b.jump((header, &[zero, seed]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let s = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[s]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let p = b.ptr_add(Flags::NONE, ptr, off);
        let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let x = b.binary(Op::Xor, Flags::NONE, s, v);
        let y = b.binary(Op::Xor, Flags::NONE, v, secret);
        let lo = b.binary(Op::Mul, Flags::NONE, x, y);
        let hi = b.binary(Op::Umulhi, Flags::NONE, x, y);
        let s2 = b.binary(Op::Xor, Flags::NONE, lo, hi);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, s2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unroll_scalar_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_unrolled, 1);
        assert!(verify(out.func.view()).is_ok());
        // `u_loop` (block 2) carries `iv0` + 1 threaded serial state `s` (2 params) and 4 contiguous `Load`s!
        let uv = out.func.view();
        assert_eq!(uv.params(Block(2)).len(), 2);
        assert_eq!(uv.block(Block(2)).filter(|&inst| uv.op(inst) == Op::Load).count(), 4);
        assert_eq!(uv.block(Block(2)).filter(|&inst| uv.op(inst) == Op::Umulhi).count(), 4);

        for &nv in &[0u128, 1, 2, 3, 4, 5, 7, 8, 9, 13, 16] {
            let args = [Val::Int(nv), Val::Int(0x1234_5678_9abc_def0)];
            let Outcome::Returned(a) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("orig wyhash eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("unroll wyhash eval");
            };
            assert_eq!(a, c, "wyhash serial unroll mismatch at n={nv}");
        }
    }

    #[test]
    fn scalar_unroll_4x_pointer_iv_and_breadth_first_interleave_arx_mix() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let arr_slot = b.slot(20 * 8, 3, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let start = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let ptr = b.slot_addr(arr_slot);
        for k in 0..20u64 {
            let v = b.iconst(Ty::I64, u128::from(k.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(17)));
            b.store(Flags::NONE, v, ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let init = b.iconst(Ty::I64, 0x1234_5678_9abc_def0);
        let golden = b.iconst(Ty::I64, 0x9e37_79b9_7f4a_7c15);
        let c13 = b.iconst(Ty::I64, 13);
        let three = b.iconst(Ty::I64, 3);
        let one = b.iconst(Ty::I64, 1);
        b.jump((header, &[start, init]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let p = b.ptr_add(Flags::NONE, ptr, off);
        let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let rot = b.binary(Op::Rotl, Flags::NONE, x, c13);
        let m = b.binary(Op::Mul, Flags::NONE, x, golden);
        let mix = b.binary(Op::Xor, Flags::NONE, rot, m);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, mix);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unroll_scalar_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_unrolled, 1);
        let uv = out.func.view();
        assert!(verify(uv).is_ok());
        // `u_loop` (block 2): running `Ty::PTR` param + 4 accumulators, ZERO `Op::Shl`, 1 `Op::PtrAdd`,
        // and 2x2 pair-interleaved `Op::Mul` instructions (2 contiguous muls per pair, reducing
        // peak simultaneous live GPRs by 2 for a frameless leaf)!
        let u_params = uv.params(Block(2));
        assert_eq!(u_params.len(), 5);
        assert_eq!(Ty(uv.tys[u_params.start]), Ty::PTR);
        assert_eq!(uv.block(Block(2)).filter(|&k| uv.op(k) == Op::Shl).count(), 0);
        assert_eq!(uv.block(Block(2)).filter(|&k| uv.op(k) == Op::PtrAdd).count(), 1);
        let mut mul_idxs: Vec<usize, TestHeap> = Vec::new_in(TestHeap);
        for k in uv.block(Block(2)) {
            if uv.op(k) == Op::Mul {
                mul_idxs.push(k);
            }
        }
        assert_eq!(mul_idxs.len(), 4);
        assert_eq!(mul_idxs[1], mul_idxs[0] + 1, "expected pair (0,1) muls contiguous");
        assert_eq!(mul_idxs[3], mul_idxs[2] + 1, "expected pair (2,3) muls contiguous");
        assert!(mul_idxs[2] > mul_idxs[1] + 1, "expected pair (0,1) ops before pair (2,3) muls");

        for &(s, nv) in &[
            (0u128, 0u128),
            (0, 4),
            (0, 17),
            (3, 3),
            (3, 6),
            (3, 7),
            (3, 8),
            (3, 19),
            (3, 20),
        ] {
            let args = [Val::Int(s), Val::Int(nv)];
            let Outcome::Returned(a) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("orig arx ptr-iv eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("unroll arx ptr-iv eval");
            };
            assert_eq!(a, c, "arx_mix ptr-iv mismatch at start={s}, n={nv}");
        }
    }

    #[test]
    fn rotate_loops_nested_bsearch_batch_rotates_outer_and_inner_and_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let arr_slot = b.slot(8 * 8, 3, SlotKind::Temp);
        let qry_slot = b.slot(8 * 8, 3, SlotKind::Temp);
        let [entry, h_q, b_q, h_bs, b_bs, end_bs, exit_all] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n_q = b.param(Ty::I64);
        let n_arr = b.param(Ty::I64);
        let arr_p = b.slot_addr(arr_slot);
        let qry_p = b.slot_addr(qry_slot);
        for k in 0..8u64 {
            let av = b.iconst(Ty::I64, u128::from(k * 10 + 5));
            let qv = b.iconst(Ty::I64, u128::from((k * 13) % 85));
            b.store(Flags::NONE, av, arr_p, (k * 8) as i32, MemAttrs::aligned(3));
            b.store(Flags::NONE, qv, qry_p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        b.jump((h_q, &[zero, zero]));

        // Outer query loop header `h_q`.
        b.begin(h_q);
        let q = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep_q = b.icmp(IntCond::Ult, q, n_q);
        b.br(Flags::NONE, keep_q, (b_q, &[]), (exit_all, &[acc]));

        // Outer body entry `b_q`: load query, enter inner binary search `h_bs`.
        b.begin(b_q);
        let q_off = b.binary(Op::Shl, Flags::NONE, q, three);
        let qp = b.ptr_add(Flags::NONE, qry_p, q_off);
        let target = b.load(Ty::I64, Flags::NONE, qp, 0, MemAttrs::aligned(3));
        b.jump((h_bs, &[zero, n_arr]));

        // Inner binary search header `h_bs`.
        b.begin(h_bs);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let keep_bs = b.icmp(IntCond::Ult, lo, hi);
        b.br(Flags::NONE, keep_bs, (b_bs, &[]), (end_bs, &[lo]));

        // Inner binary search body `b_bs`.
        b.begin(b_bs);
        let sum_lh = b.binary(Op::Add, Flags::NONE, lo, hi);
        let mid = b.binary(Op::Lshr, Flags::NONE, sum_lh, one);
        let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let mp = b.ptr_add(Flags::NONE, arr_p, m_off);
        let mv = b.load(Ty::I64, Flags::NONE, mp, 0, MemAttrs::aligned(3));
        let lt = b.icmp(IntCond::Ult, mv, target);
        let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
        let lo2 = b.select(Flags::NONE, lt, mid1, lo);
        let hi2 = b.select(Flags::NONE, lt, hi, mid);
        b.jump((h_bs, &[lo2, hi2]));

        // Outer latch `end_bs`.
        b.begin(end_bs);
        let res_lo = b.param(Ty::I64);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, res_lo);
        let q2 = b.binary(Op::Add, Flags::NONE, q, one);
        b.jump((h_q, &[q2, acc2]));

        b.begin(exit_all);
        let final_acc = b.param(Ty::I64);
        b.ret(&[final_acc]);
        let func = b.finish_checked().unwrap();

        let out = rotate_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_rotated, 2, "both outer h_q and inner h_bs must rotate");
        let rv = out.func.view();
        assert!(verify(rv).is_ok());
        assert_eq!(
            (0..rv.len()).filter(|&k| rv.op(k) == Op::Jump).count(),
            0,
            "rotated nested bsearch_batch must have ZERO unconditional Jump back-edges"
        );

        for &(nq, na) in &[
            (0u128, 0u128),
            (0, 8),
            (1, 0),
            (1, 1),
            (1, 8),
            (3, 5),
            (8, 8),
        ] {
            let args = [Val::Int(nq), Val::Int(na)];
            let Outcome::Returned(a) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("orig bsearch_batch eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("rotated bsearch_batch eval");
            };
            assert_eq!(a, c, "bsearch_batch rotation mismatch at n_q={nq}, n_arr={na}");
        }
    }

    #[test]
    fn rotate_loops_multi_latch_if_else_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, even_b, odd_b, exit] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let seven = b.iconst(Ty::I64, 7);
        b.jump((header, &[z, z]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let lsb = b.binary(Op::And, Flags::NONE, i, one);
        let is_even = b.icmp(IntCond::Eq, lsb, z);
        b.br(Flags::NONE, is_even, (even_b, &[]), (odd_b, &[]));

        b.begin(even_b);
        let ae = b.binary(Op::Add, Flags::NONE, acc, three);
        let ie = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[ie, ae]));

        b.begin(odd_b);
        let ao = b.binary(Op::Xor, Flags::NONE, acc, seven);
        let io = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[io, ao]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let out = rotate_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_rotated, 1);
        let rv = out.func.view();
        assert!(verify(rv).is_ok());
        assert_eq!((0..rv.len()).filter(|&k| rv.op(k) == Op::Jump).count(), 0);

        for &nv in &[0u128, 1, 2, 3, 5, 11, 20] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv)], 50_000, TestHeap).unwrap() else {
                panic!("orig multi-latch eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &[Val::Int(nv)], 50_000, TestHeap).unwrap() else {
                panic!("rotated multi-latch eval");
            };
            assert_eq!(a, c, "multi-latch rotation mismatch at n={nv}");
        }
    }

    #[test]
    fn rotate_loops_preheader_br_rotates_unswitched_outer_loops_and_matches_eval() {
        // Mirrors post-`unswitch_loops` `bsearch_batch`: `entry` ends in `Br(0 < n_arr, L_true(0, 0), L_false(0, 0))`!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, lt_hdr, lt_body, lt_inner, lt_latch, lf_hdr, lf_body, exit_all] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n_q = b.param(Ty::I64);
        let n_arr = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let seven = b.iconst(Ty::I64, 7);
        let has_arr = b.icmp(IntCond::Ult, zero, n_arr);
        b.br(Flags::NONE, has_arr, (lt_hdr, &[zero, zero]), (lf_hdr, &[zero, zero]));

        // L_true outer header (preheader = entry via BrThen).
        b.begin(lt_hdr);
        let q_t = b.param(Ty::I64);
        let acc_t = b.param(Ty::I64);
        let keep_qt = b.icmp(IntCond::Ult, q_t, n_q);
        b.br(Flags::NONE, keep_qt, (lt_body, &[]), (exit_all, &[acc_t]));

        b.begin(lt_body);
        b.jump((lt_inner, &[zero, n_arr]));

        // Already-rotated inner loop inside L_true.
        b.begin(lt_inner);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let lo_next = b.binary(Op::Add, Flags::NONE, lo, one);
        let keep_in = b.icmp(IntCond::Ult, lo_next, hi);
        b.br(Flags::NONE, keep_in, (lt_inner, &[lo_next, hi]), (lt_latch, &[lo_next]));

        b.begin(lt_latch);
        let res_in = b.param(Ty::I64);
        let mix_t = b.binary(Op::Add, Flags::NONE, acc_t, res_in);
        let qt2 = b.binary(Op::Add, Flags::NONE, q_t, one);
        b.jump((lt_hdr, &[qt2, mix_t]));

        // L_false outer header (preheader = entry via BrElse).
        b.begin(lf_hdr);
        let q_f = b.param(Ty::I64);
        let acc_f = b.param(Ty::I64);
        let keep_qf = b.icmp(IntCond::Ult, q_f, n_q);
        b.br(Flags::NONE, keep_qf, (lf_body, &[]), (exit_all, &[acc_f]));

        b.begin(lf_body);
        let acc_f2 = b.binary(Op::Xor, Flags::NONE, acc_f, seven);
        let qf2 = b.binary(Op::Add, Flags::NONE, q_f, one);
        b.jump((lf_hdr, &[qf2, acc_f2]));

        b.begin(exit_all);
        let final_acc = b.param(Ty::I64);
        b.ret(&[final_acc]);
        let func = b.finish_checked().unwrap();

        let out = rotate_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_rotated, 2, "both BrThen L_true and BrElse L_false outer loops must rotate");
        let rv = out.func.view();
        assert!(verify(rv).is_ok());
        // Only the entry jump `lt_body -> lt_inner` remains; ZERO outer-loop Jump back-edges!
        assert_eq!((0..rv.len()).filter(|&k| rv.op(k) == Op::Jump).count(), 1);

        for &(nq, na) in &[
            (0u128, 0u128),
            (0, 5),
            (1, 0),
            (1, 1),
            (1, 5),
            (4, 0),
            (4, 6),
            (7, 3),
        ] {
            let args = [Val::Int(nq), Val::Int(na)];
            let Outcome::Returned(a) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("orig unswitched outer eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("rotated unswitched outer eval");
            };
            assert_eq!(a, c, "unswitched preheader-Br rotation mismatch at n_q={nq}, n_arr={na}");
        }
    }
}

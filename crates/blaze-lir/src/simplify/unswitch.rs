//! Loop Unswitching (`unswitch_loops`): hoists loop-invariant branches completely out of loops.
//!
//! When a natural loop contains `Br(cond, then, else)` where `cond` is loop-invariant (for
//! example, a PGO speculative devirtualization guard `fp == hot_cb` hoisted by LICM, or an
//! invariant mode flag), Loop Unswitching clones the loop into two specialized loops (`L_true`
//! with `cond = true`, `L_false` with `cond = false`) behind a single preheader branch.
//!
//! Combined with `speculative_devirt` + `lto` inlining + `dead` + `coalesce_cfg`, a hot
//! `dyn Trait` / function-pointer loop becomes a **branchless, call-free, frameless leaf loop**
//! that unlocks 4x-unrolled 128-bit SIMD auto-vectorization and 4x scalar unrolling!
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, coalesce_cfg, dead, each, filled, Item},
};
use crate::{Block, Func, Op, Ty, View, verify};

/// Telemetry returned by [`unswitch_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct UnswitchStats {
    pub loops_unswitched: u32,
}

/// Output of [`unswitch_loops`].
#[derive(Debug)]
pub struct UnswitchOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: UnswitchStats,
}

/// Maximum total instructions inside a loop eligible for cloning during unswitching.
const MAX_UNSWITCH_INSTS: usize = 96;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
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

struct UnswitchPlan<A: Allocator> {
    preheader: u32,
    header: u32,
    /// Blocks belonging to the natural loop (sorted ascending, `loop_blocks[0] == header`).
    loop_blocks: Vec<u32, A>,
    /// The loop-invariant condition value tested by `br_inst`.
    inv_cond: u32,
    /// The `Op::Br` terminator instruction inside the loop testing `inv_cond`.
    br_inst: usize,
    /// Unique external exit block (if any) that receives direct def-uses from `loop_blocks`.
    direct_exit: Option<u32>,
    /// Values defined in `loop_blocks` that are directly used outside `loop_blocks` (promoted to exit params).
    escaped_vals: Vec<u32, A>,
}

fn find_unswitch_candidate<A: Allocator + Copy>(view: &View<'_>, alloc: A) -> Option<UnswitchPlan<A>> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 3 {
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

    let mut in_loop = filled(blocks, false, &alloc);
    for h in 1..blocks {
        let header = to_u32(h);
        let mut latches: Vec<u32, A> = Vec::new_in(alloc);
        let mut outside_preds = 0u32;
        let mut preheader = NONE;
        for &p in preds.get(h).map(Vec::as_slice).unwrap_or(&[]) {
            if p >= header {
                latches.push(p);
            } else {
                outside_preds += 1;
                preheader = p;
            }
        }
        if latches.is_empty() || outside_preds != 1 || preheader == NONE {
            continue;
        }
        if view.op(view.terminator(Block(preheader))) != Op::Jump {
            continue;
        }

        // Collect all blocks in the natural loop.
        let mut loop_blocks: Vec<u32, A> = Vec::new_in(alloc);
        let mut work: Vec<u32, A> = Vec::new_in(alloc);
        set_at(&mut in_loop, h, true);
        loop_blocks.push(header);
        for &l in &latches {
            if !at(&in_loop, to_idx(l)) {
                set_at(&mut in_loop, to_idx(l), true);
                loop_blocks.push(l);
                work.push(l);
            }
        }
        let mut malformed = false;
        while let Some(cur) = work.pop() {
            for &p in preds.get(to_idx(cur)).map(Vec::as_slice).unwrap_or(&[]) {
                if p < header {
                    malformed = true;
                    break;
                }
                if !at(&in_loop, to_idx(p)) {
                    set_at(&mut in_loop, to_idx(p), true);
                    loop_blocks.push(p);
                    work.push(p);
                }
            }
            if malformed {
                break;
            }
        }
        if malformed {
            for &bb in &loop_blocks {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        }
        loop_blocks.sort_unstable();

        // Check total instruction budget and ensure no landing blocks.
        let mut total_insts = 0usize;
        let mut has_landing = false;
        for &bb in &loop_blocks {
            if view.block_kind(Block(bb)).is_landing() {
                has_landing = true;
                break;
            }
            total_insts += view.block(Block(bb)).len();
        }
        if has_landing || total_insts > MAX_UNSWITCH_INSTS {
            for &bb in &loop_blocks {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        }

        // Look for an internal `Op::Br` whose condition is defined outside the loop (`<= preheader`)
        // and whose BOTH targets are inside the loop (or at least one inside, keeping the loop cyclic).
        let mut found_br: Option<(u32, usize)> = None;
        for &bb in &loop_blocks {
            let term = view.terminator(Block(bb));
            if view.op(term) == Op::Br {
                let idx = to_idx(at(view.data, term));
                let cv = at(view.extra, idx);
                let cb = at(&inst_block, to_idx(cv));
                if !at(&in_loop, to_idx(cb)) && cb <= preheader {
                    found_br = Some((cv, term));
                    break;
                }
            }
        }
        let Some((inv_cond, br_inst)) = found_br else {
            for &bb in &loop_blocks {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        };

        // Check if any loop-defined value is directly used outside the loop (without an exit param).
        let mut exit_blocks: Vec<u32, A> = Vec::new_in(alloc);
        for &bb in &loop_blocks {
            let term = view.terminator(Block(bb));
            each(view, term, |item| {
                if let Item::Edge(edge) = item
                    && !at(&in_loop, to_idx(edge.block))
                    && exit_blocks.scan_for(&edge.block).is_none()
                {
                    exit_blocks.push(edge.block);
                }
            });
        }

        let mut escaped_vals: Vec<u32, A> = Vec::new_in(alloc);
        for b in 0..blocks {
            if at(&in_loop, b) {
                continue;
            }
            for inst in view.block(Block(to_u32(b))) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item
                        && at(&in_loop, to_idx(at(&inst_block, to_idx(v))))
                        && escaped_vals.scan_for(&v).is_none()
                    {
                        escaped_vals.push(v);
                    }
                });
            }
        }

        // If values escape directly without exit block params, we can promote them cleanly iff
        // there is a single exit block whose ONLY predecessors are inside the loop!
        let direct_exit = if escaped_vals.is_empty() {
            None
        } else if exit_blocks.len() == 1 {
            let ex = exit_blocks[0];
            let all_preds_in_loop = preds
                .get(to_idx(ex))
                .is_some_and(|ps| ps.iter().all(|&p| at(&in_loop, to_idx(p))));
            if all_preds_in_loop {
                Some(ex)
            } else {
                for &bb in &loop_blocks {
                    set_at(&mut in_loop, to_idx(bb), false);
                }
                continue;
            }
        } else {
            for &bb in &loop_blocks {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        };

        for &bb in &loop_blocks {
            set_at(&mut in_loop, to_idx(bb), false);
        }
        return Some(UnswitchPlan {
            preheader,
            header,
            loop_blocks,
            inv_cond,
            br_inst,
            direct_exit,
            escaped_vals,
        });
    }
    None
}

fn emit_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    direct_exit: Option<u32>,
    escaped_vals: &[u32],
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
                        let b_u32 = at(view.extra, p);
                        let b = to_idx(b_u32);
                        let n = to_idx(at(view.extra, p + 1));
                        let extra_esc = if direct_exit == Some(b_u32) { escaped_vals.len() } else { 0 };
                        func.extra.push(at(blk_map, b));
                        func.extra.push(to_u32(n + extra_esc));
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        for &ev in escaped_vals.iter().take(extra_esc) {
                            func.extra.push(at(val_map, to_idx(ev)));
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
                            let b_u32 = at(view.extra, p);
                            let b = to_idx(b_u32);
                            let an = to_idx(at(view.extra, p + 1));
                            let extra_esc = if direct_exit == Some(b_u32) { escaped_vals.len() } else { 0 };
                            func.extra.push(at(blk_map, b));
                            func.extra.push(to_u32(an + extra_esc));
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                            }
                            for &ev in escaped_vals.iter().take(extra_esc) {
                                func.extra.push(at(val_map, to_idx(ev)));
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

fn emit_folded_br<A: Allocator>(
    view: &View<'_>,
    br_inst: usize,
    take_then: bool,
    val_map: &[u32],
    blk_map: &[u32],
    direct_exit: Option<u32>,
    escaped_vals: &[u32],
    func: &mut Func<A>,
) {
    let idx = to_idx(at(view.data, br_inst));
    let t_blk = at(view.extra, idx + 1);
    let t_argc = to_idx(at(view.extra, idx + 2));
    let e_at = idx + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));
    let (tgt_old, args_start, argc) = if take_then {
        (t_blk, idx + 3, t_argc)
    } else {
        (e_blk, e_at + 2, e_argc)
    };
    let extra_esc = if direct_exit == Some(tgt_old) { escaped_vals.len() } else { 0 };
    let dst_at = to_u32(func.extra.len());
    func.extra.push(at(blk_map, to_idx(tgt_old)));
    func.extra.push(to_u32(argc + extra_esc));
    for k in 0..argc {
        func.extra.push(at(val_map, to_idx(at(view.extra, args_start + k))));
    }
    for &ev in escaped_vals.iter().take(extra_esc) {
        func.extra.push(at(val_map, to_idx(ev)));
    }
    func.ops.push(op_byte(Op::Jump));
    func.flags.push(0);
    func.tys.push(Ty::NONE.0);
    func.data.push(dst_at);
    func.attrs.push(0);
    func.locs.push(at(view.locs, br_inst));
}

/// Hoists loop-invariant branches out of natural loops (up to `max_loops` unswitches), folding the
/// dead arm out of each specialized clone and running `dead` + `coalesce_cfg`.
pub fn unswitch_loops<A: Allocator + Copy>(
    view: View<'_>,
    max_loops: u32,
    alloc: A,
) -> Result<UnswitchOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut total = 0u32;

    for _ in 0..max_loops {
        let Some(plan) = find_unswitch_candidate(&cur.view(), alloc) else {
            break;
        };
        let v = cur.view();
        let blocks = v.blocks();
        let len = v.len();

        let mut in_loop = filled(blocks, false, &alloc);
        for &bb in &plan.loop_blocks {
            set_at(&mut in_loop, to_idx(bb), true);
        }
        let k = plan.loop_blocks.len();

        // New block layout:
        //   0..=preheader -> unchanged
        //   L_true blocks (k blocks, immediately after preheader -> hot fast loop!)
        //   L_false blocks (k blocks -> cold fallback loop!)
        //   remaining non-loop blocks > preheader (shifted cleanly)
        let mut blk_map_outside = filled(blocks, NONE, &alloc);
        let mut blk_map_true = filled(blocks, NONE, &alloc);
        let mut blk_map_false = filled(blocks, NONE, &alloc);

        let preheader_idx = to_idx(plan.preheader);
        let mut next_blk = 0u32;
        for b in 0..=preheader_idx {
            set_at(&mut blk_map_outside, b, next_blk);
            set_at(&mut blk_map_true, b, next_blk);
            set_at(&mut blk_map_false, b, next_blk);
            next_blk += 1;
        }
        for &lb in &plan.loop_blocks {
            set_at(&mut blk_map_true, to_idx(lb), next_blk);
            next_blk += 1;
        }
        for &lb in &plan.loop_blocks {
            set_at(&mut blk_map_false, to_idx(lb), next_blk);
            next_blk += 1;
        }
        for b in (preheader_idx + 1)..blocks {
            if !at(&in_loop, b) {
                set_at(&mut blk_map_outside, b, next_blk);
                set_at(&mut blk_map_true, b, next_blk);
                set_at(&mut blk_map_false, b, next_blk);
                next_blk += 1;
            }
        }
        let _ = k;

        let mut func = v.to_func_in(alloc);
        func.ops.clear();
        func.flags.clear();
        func.tys.clear();
        func.data.clear();
        func.extra.clear();
        func.attrs.clear();
        func.locs.clear();
        func.block_starts.clear();
        func.block_kinds.clear();

        let mut val_map_out = filled(len, NONE, &alloc);
        let mut val_map_true = filled(len, NONE, &alloc);
        let mut val_map_false = filled(len, NONE, &alloc);

        // 1. Emit blocks 0..=preheader; rewrite `preheader`'s `Jump` into `Br(inv_cond, L_true, L_false)`.
        for b in 0..=preheader_idx {
            let b_u32 = to_u32(b);
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(v.block_kinds, b));
            let range = v.block(Block(b_u32));
            if b_u32 == plan.preheader {
                let term = range.end - 1;
                for inst in range.start..term {
                    let nv = emit_remapped(&v, inst, &val_map_out, &blk_map_outside, None, &[], &mut func);
                    set_at(&mut val_map_out, inst, nv);
                    set_at(&mut val_map_true, inst, nv);
                    set_at(&mut val_map_false, inst, nv);
                }
                let jat = to_idx(at(v.data, term));
                let argc_u32 = at(v.extra, jat + 1);
                let argc = to_idx(argc_u32);
                let br_at = to_u32(func.extra.len());
                func.extra.push(at(&val_map_out, to_idx(plan.inv_cond)));
                func.extra.push(at(&blk_map_true, to_idx(plan.header)));
                func.extra.push(argc_u32);
                for a in 0..argc {
                    func.extra.push(at(&val_map_out, to_idx(at(v.extra, jat + 2 + a))));
                }
                func.extra.push(at(&blk_map_false, to_idx(plan.header)));
                func.extra.push(argc_u32);
                for a in 0..argc {
                    func.extra.push(at(&val_map_out, to_idx(at(v.extra, jat + 2 + a))));
                }
                func.ops.push(op_byte(Op::Br));
                func.flags.push(at(v.flags, plan.br_inst));
                func.tys.push(Ty::NONE.0);
                func.data.push(br_at);
                func.attrs.push(0);
                func.locs.push(at(v.locs, term));
            } else {
                for inst in range {
                    let nv = emit_remapped(&v, inst, &val_map_out, &blk_map_outside, None, &[], &mut func);
                    set_at(&mut val_map_out, inst, nv);
                    set_at(&mut val_map_true, inst, nv);
                    set_at(&mut val_map_false, inst, nv);
                }
            }
        }

        // 2. Emit `L_true` clone (folding `br_inst` -> `then`!).
        for &lb in &plan.loop_blocks {
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
            let range = v.block(Block(lb));
            for inst in range {
                if inst == plan.br_inst {
                    emit_folded_br(
                        &v,
                        inst,
                        true,
                        &val_map_true,
                        &blk_map_true,
                        plan.direct_exit,
                        &plan.escaped_vals,
                        &mut func,
                    );
                } else {
                    let nv = emit_remapped(
                        &v,
                        inst,
                        &val_map_true,
                        &blk_map_true,
                        plan.direct_exit,
                        &plan.escaped_vals,
                        &mut func,
                    );
                    set_at(&mut val_map_true, inst, nv);
                }
            }
        }

        // 3. Emit `L_false` clone (folding `br_inst` -> `else`!).
        for &lb in &plan.loop_blocks {
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
            let range = v.block(Block(lb));
            for inst in range {
                if inst == plan.br_inst {
                    emit_folded_br(
                        &v,
                        inst,
                        false,
                        &val_map_false,
                        &blk_map_false,
                        plan.direct_exit,
                        &plan.escaped_vals,
                        &mut func,
                    );
                } else {
                    let nv = emit_remapped(
                        &v,
                        inst,
                        &val_map_false,
                        &blk_map_false,
                        plan.direct_exit,
                        &plan.escaped_vals,
                        &mut func,
                    );
                    set_at(&mut val_map_false, inst, nv);
                }
            }
        }

        // 4. Emit remaining non-loop blocks after `preheader` (inserting promoted params on `direct_exit`!).
        for b in (preheader_idx + 1)..blocks {
            if at(&in_loop, b) {
                continue;
            }
            let b_u32 = to_u32(b);
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(v.block_kinds, b));
            let range = v.block(Block(b_u32));
            let params = v.params(Block(b_u32));
            for inst in params.start..params.end {
                let nv = emit_remapped(&v, inst, &val_map_out, &blk_map_outside, None, &[], &mut func);
                set_at(&mut val_map_out, inst, nv);
            }
            if plan.direct_exit == Some(b_u32) {
                for &ev in &plan.escaped_vals {
                    let p_idx = to_u32(func.ops.len());
                    func.ops.push(op_byte(Op::Param));
                    func.flags.push(0);
                    func.tys.push(at(v.tys, to_idx(ev)));
                    func.data.push(0);
                    func.attrs.push(0);
                    func.locs.push(at(v.locs, to_idx(ev)));
                    set_at(&mut val_map_out, to_idx(ev), p_idx);
                }
            }
            for inst in params.end..range.end {
                let nv = emit_remapped(&v, inst, &val_map_out, &blk_map_outside, None, &[], &mut func);
                set_at(&mut val_map_out, inst, nv);
            }
        }

        verify(func.view()).map_err(SimplifyError::Verify)?;
        let cleaned = dead(func.view(), alloc)?;
        cur = coalesce_cfg(cleaned.view(), alloc)?;
        total += 1;
    }

    Ok(UnswitchOutput {
        func: cur,
        stats: UnswitchStats { loops_unswitched: total },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        Flags, IntCond,
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv},
    };

    #[test]
    fn unswitch_hoists_invariant_branch_out_of_loop_and_preserves_semantics() {
        // `for i in 0..n { acc += if mode { i * 3 } else { i + 5 }; }`
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I1)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, fast, slow, latch, exit] = [(); 7].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let mode = b.param(Ty::I1);
        let z = b.iconst(Ty::I64, 0);
        b.jump((header, &[z, z]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let c = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, c, (body, &[]), (exit, &[]));
        b.begin(body);
        b.br(Flags::NONE, mode, (fast, &[]), (slow, &[]));
        b.begin(fast);
        let k3 = b.iconst(Ty::I64, 3);
        let v1 = b.binary(Op::Mul, Flags::NONE, i, k3);
        b.jump((latch, &[v1]));
        b.begin(slow);
        let k5 = b.iconst(Ty::I64, 5);
        let v2 = b.binary(Op::Add, Flags::NONE, i, k5);
        b.jump((latch, &[v2]));
        b.begin(latch);
        let delta = b.param(Ty::I64);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, delta);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));
        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let out = unswitch_loops(func.view(), 2, TestHeap).unwrap();
        assert_eq!(out.stats.loops_unswitched, 1);

        // Verify evaluator equivalence across both `mode = true` and `mode = false`!
        for &m in &[0u128, 1u128] {
            for &nv in &[0u128, 1, 7, 25] {
                let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv), Val::Int(m)], 10_000, TestHeap).unwrap() else {
                    panic!("orig eval failed");
                };
                let Outcome::Returned(b) = eval::run_in(out.func.view(), &[Val::Int(nv), Val::Int(m)], 10_000, TestHeap).unwrap() else {
                    panic!("unswitched eval failed");
                };
                assert_eq!(a, b, "unswitch mismatch for n={nv}, mode={m}");
            }
        }
    }
}

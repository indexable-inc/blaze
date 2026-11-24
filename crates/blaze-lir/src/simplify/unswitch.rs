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
use crate::{Block, BlockKind, Func, Op, Ty, View, verify};

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
const MAX_UNSWITCH_INSTS: usize = 128;

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

#[inline]
fn is_pure_const(view: &View<'_>, inst: usize) -> bool {
    matches!(view.op(inst), Op::Iconst | Op::IconstWide) && at(view.flags, inst) == 0
}

#[inline]
fn is_pure_guard_op(op: Op) -> bool {
    matches!(
        op,
        Op::Icmp
            | Op::Fcmp
            | Op::Iconst
            | Op::IconstWide
            | Op::Add
            | Op::Sub
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Zext
            | Op::Sext
            | Op::Trunc
    )
}

#[inline]
fn is_outer_invariant(
    view: &View<'_>,
    val: u32,
    in_loop: &[bool],
    inst_block: &[u32],
    preheader: u32,
) -> bool {
    let vi = to_idx(val);
    let vb = at(inst_block, vi);
    (!at(in_loop, to_idx(vb)) && vb <= preheader) || is_pure_const(view, vi)
}

fn for_each_use(view: &View<'_>, inst: usize, mut f: impl FnMut(u32)) {
    each(view, inst, |item| match item {
        Item::Value(v) => f(v),
        Item::Edge(edge) => {
            if let Some((start, count)) = edge.args {
                for k in 0..count {
                    f(at(view.extra, start + k));
                }
            }
        }
    });
}

enum UnswitchKind<A: Allocator> {
    /// Loop-invariant `Op::Br` inside `loop_blocks`.
    /// When `hoist_cmp` is `Some(cmp_inst)`, `cmp_inst` is a pure, flag-free `Icmp`/`Fcmp`
    /// defined inside `loop_blocks` whose operands are outer-loop-invariant (or pure constants)
    /// and is materialized in `preheader`.
    InvariantBr {
        inv_cond: u32,
        br_inst: usize,
        hoist_cmp: Option<usize>,
    },
    /// Nested inner loop whose first-iteration header guard `cond(init_args)` is
    /// outer-loop-invariant (e.g. `0 < n` on `for q in 0..m { while lo < hi { ... } }`).
    ///
    /// - `L_true` (`cond(init_args)` takes `inner_body`): enters `inner_body` directly from
    ///   `outer_entry_blk` with `init_args`, rotating `inner_header` to the bottom latch check.
    /// - `L_false` (`cond(init_args)` takes `inner_exit`): skips `inner_blocks` completely,
    ///   jumping straight from `outer_entry_blk` to `inner_exit` with substituted `exit_args`.
    InnerLoopGuard {
        outer_entry_blk: u32,
        inner_header: u32,
        inner_body: u32,
        inner_exit: u32,
        body_is_then: bool,
        cond_inst: u32,
        inner_blocks: Vec<u32, A>,
        init_args: Vec<u32, A>,
        exit_args: Vec<u32, A>,
    },
}

#[inline]
fn br_arm_target(view: &View<'_>, br_inst: usize, take_then: bool) -> (u32, usize, usize) {
    let idx = to_idx(at(view.data, br_inst));
    let t_blk = at(view.extra, idx + 1);
    let t_argc = to_idx(at(view.extra, idx + 2));
    let e_at = idx + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));
    if take_then { (t_blk, idx + 3, t_argc) } else { (e_blk, e_at + 2, e_argc) }
}

struct UnswitchPlan<A: Allocator> {
    preheader: u32,
    header: u32,
    /// Blocks belonging to the natural loop (sorted ascending, `loop_blocks[0] == header`).
    loop_blocks: Vec<u32, A>,
    /// Blocks emitted in the `L_true` copy (in emission order).
    l_true_blocks: Vec<u32, A>,
    /// Blocks emitted in the `L_false` copy (in emission order).
    l_false_blocks: Vec<u32, A>,
    /// Outside blocks (`!in_loop`) reachable from `L_true` (empty when per-copy pruning is unused).
    reach_out_true: Vec<bool, A>,
    /// Outside blocks (`!in_loop`) reachable from `L_false` (empty when per-copy pruning is unused).
    reach_out_false: Vec<bool, A>,
    kind: UnswitchKind<A>,
    /// Block index defining each instruction in `view`.
    inst_block: Vec<u32, A>,
    /// Unique external exit block (if any) that receives direct def-uses from both copies.
    direct_exit: Option<u32>,
    /// Values defined in `loop_blocks` promoted to `direct_exit` block params.
    escaped_vals: Vec<u32, A>,
}

fn find_inner_loop_guard_candidate<A: Allocator + Copy>(
    view: &View<'_>,
    preheader: u32,
    header: u32,
    loop_blocks: &[u32],
    in_loop: &[bool],
    inst_block: &[u32],
    preds: &[Vec<u32, A>],
    escaped_vals: &[u32],
    alloc: A,
) -> Option<UnswitchKind<A>> {
    let blocks = view.blocks();
    let mut in_inner = filled(blocks, false, &alloc);

    for &ih in loop_blocks {
        if ih == header || view.block_kind(Block(ih)) != BlockKind::Normal {
            continue;
        }
        let ih_idx = to_idx(ih);
        let mut outside_preds = 0u32;
        let mut outer_entry_blk = NONE;
        let mut inner_latches: Vec<u32, A> = Vec::new_in(alloc);
        let mut self_loop = false;
        for &p in preds.get(ih_idx).map(Vec::as_slice).unwrap_or(&[]) {
            if p == ih {
                self_loop = true;
                break;
            } else if p < ih {
                outside_preds += 1;
                outer_entry_blk = p;
            } else {
                inner_latches.push(p);
            }
        }
        if self_loop
            || outside_preds != 1
            || outer_entry_blk == NONE
            || inner_latches.is_empty()
            || !at(in_loop, to_idx(outer_entry_blk))
            || !inner_latches.iter().all(|&l| at(in_loop, to_idx(l)))
        {
            continue;
        }

        // `outer_entry_blk` must enter `ih` via `Op::Jump(ih, init_args)`.
        let oe_term = view.terminator(Block(outer_entry_blk));
        if view.op(oe_term) != Op::Jump {
            continue;
        }
        let oe_at = to_idx(at(view.data, oe_term));
        if at(view.extra, oe_at) != ih {
            continue;
        }
        let ih_params = view.params(Block(ih));
        let n_params = ih_params.len();
        if n_params == 0 || to_idx(at(view.extra, oe_at + 1)) != n_params {
            continue;
        }
        let mut init_args: Vec<u32, A> = Vec::with_capacity_in(n_params, alloc);
        for k in 0..n_params {
            init_args.push(at(view.extra, oe_at + 2 + k));
        }

        // Collect all blocks in the inner natural sub-loop rooted at `ih`.
        let mut inner_blocks: Vec<u32, A> = Vec::new_in(alloc);
        let mut work: Vec<u32, A> = Vec::new_in(alloc);
        set_at(&mut in_inner, ih_idx, true);
        inner_blocks.push(ih);
        for &l in &inner_latches {
            if !at(&in_inner, to_idx(l)) {
                set_at(&mut in_inner, to_idx(l), true);
                inner_blocks.push(l);
                work.push(l);
            }
        }
        let mut bad_inner = false;
        while let Some(cur) = work.pop() {
            for &p in preds.get(to_idx(cur)).map(Vec::as_slice).unwrap_or(&[]) {
                if p < ih || !at(in_loop, to_idx(p)) {
                    bad_inner = true;
                    break;
                }
                if !at(&in_inner, to_idx(p)) {
                    set_at(&mut in_inner, to_idx(p), true);
                    inner_blocks.push(p);
                    work.push(p);
                }
            }
            if bad_inner {
                break;
            }
        }
        if bad_inner || at(&in_inner, to_idx(outer_entry_blk)) {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }
        inner_blocks.sort_unstable();

        // Parse `ih`'s `Op::Br(cond_inst, (t_blk, t_args), (e_blk, e_args))`.
        let ih_term = view.terminator(Block(ih));
        if view.op(ih_term) != Op::Br {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }
        let hat = to_idx(at(view.data, ih_term));
        let cond_inst = at(view.extra, hat);
        let t_blk = at(view.extra, hat + 1);
        let t_argc = to_idx(at(view.extra, hat + 2));
        let e_at = hat + 3 + t_argc;
        let e_blk = at(view.extra, e_at);
        let e_argc = to_idx(at(view.extra, e_at + 1));

        let (inner_body, inner_exit, body_is_then, body_argc, exit_args_start, exit_argc) =
            if at(&in_inner, to_idx(t_blk)) && t_blk != ih && !at(&in_inner, to_idx(e_blk)) {
                (t_blk, e_blk, true, t_argc, e_at + 2, e_argc)
            } else if at(&in_inner, to_idx(e_blk)) && e_blk != ih && !at(&in_inner, to_idx(t_blk)) {
                (e_blk, t_blk, false, e_argc, hat + 3, t_argc)
            } else {
                for &ib in &inner_blocks {
                    set_at(&mut in_inner, to_idx(ib), false);
                }
                continue;
            };

        // `inner_body` must have `ih` as its sole predecessor and carry no original block params.
        if body_argc != 0
            || !view.params(Block(inner_body)).is_empty()
            || view.block_kind(Block(inner_body)) != BlockKind::Normal
            || preds.get(to_idx(inner_body)).map(Vec::as_slice) != Some(&[ih])
        {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }

        // Verify `ih`'s body is tiny, pure, flag-free, uses at least one `ih` param, and
        // depends ONLY on outer-loop-invariant values (or `ih` params whose `init_args` are
        // outer-loop-invariant).
        let ih_range = view.block(Block(ih));
        let h_body_start = ih_params.end;
        let h_body_end = ih_range.end.saturating_sub(1);
        let cond_idx = to_idx(cond_inst);
        if h_body_end <= h_body_start
            || h_body_end - h_body_start > 4
            || cond_idx < h_body_start
            || cond_idx >= h_body_end
        {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }
        let mut guard_ok = true;
        let mut uses_ih_param = false;
        for hi in h_body_start..h_body_end {
            if !is_pure_guard_op(view.op(hi)) || at(view.flags, hi) != 0 {
                guard_ok = false;
                break;
            }
            for_each_use(view, hi, |v| {
                let vi = to_idx(v);
                if vi >= h_body_start && vi < hi {
                    // Defined earlier inside `ih`'s guard body.
                } else if vi >= ih_params.start && vi < ih_params.end {
                    uses_ih_param = true;
                    let init_v = at(&init_args, vi - ih_params.start);
                    if !is_outer_invariant(view, init_v, in_loop, inst_block, preheader) {
                        guard_ok = false;
                    }
                } else if !is_outer_invariant(view, v, in_loop, inst_block, preheader) {
                    guard_ok = false;
                }
            });
            if !guard_ok {
                break;
            }
        }
        if !guard_ok || !uses_ih_param {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }

        // Collect `exit_args` and ensure every non-`ih_param` exit arg is available at
        // `outer_entry_blk` in `L_false` (and not a non-param instruction of `ih`).
        let mut exit_args: Vec<u32, A> = Vec::with_capacity_in(exit_argc, alloc);
        let mut exit_args_ok = true;
        for k in 0..exit_argc {
            let ea = at(view.extra, exit_args_start + k);
            let eai = to_idx(ea);
            let eab = at(inst_block, eai);
            let is_ih_param = eab == ih && eai >= ih_params.start && eai < ih_params.end;
            if !is_ih_param {
                let ok_before_entry = (!at(in_loop, to_idx(eab)) && eab <= preheader)
                    || (at(in_loop, to_idx(eab)) && !at(&in_inner, to_idx(eab)) && eab <= outer_entry_blk);
                if !ok_before_entry {
                    exit_args_ok = false;
                    break;
                }
            }
            exit_args.push(ea);
        }
        if !exit_args_ok {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }

        // Ensure `ih` is the sole exiting block of `inner_blocks` (no early exits from body).
        let mut exits_ok = true;
        for &ib in &inner_blocks {
            if ib == ih {
                continue;
            }
            let term = view.terminator(Block(ib));
            each(view, term, |item| {
                if let Item::Edge(edge) = item
                    && !at(&in_inner, to_idx(edge.block))
                {
                    exits_ok = false;
                }
            });
            if !exits_ok {
                break;
            }
        }
        // Reject if any outer-loop escaped value lives inside `inner_blocks`.
        if exits_ok {
            for &ev in escaped_vals {
                if at(&in_inner, to_idx(at(inst_block, to_idx(ev)))) {
                    exits_ok = false;
                    break;
                }
            }
        }
        if !exits_ok {
            for &ib in &inner_blocks {
                set_at(&mut in_inner, to_idx(ib), false);
            }
            continue;
        }

        // Build exact `L_true` emission positions to guarantee def-before-use ordering and
        // ensure no illegal escape of `inner_blocks` defs.
        let mut pos_true = filled(blocks, NONE, &alloc);
        let mut next_pos = 0u32;
        for b in 0..=to_idx(preheader) {
            set_at(&mut pos_true, b, next_pos);
            next_pos += 1;
        }
        for &lb in loop_blocks {
            if lb < ih && !at(&in_inner, to_idx(lb)) {
                set_at(&mut pos_true, to_idx(lb), next_pos);
                next_pos += 1;
            }
        }
        set_at(&mut pos_true, to_idx(inner_body), next_pos);
        next_pos += 1;
        for &ib in &inner_blocks {
            if ib != ih && ib != inner_body {
                set_at(&mut pos_true, to_idx(ib), next_pos);
                next_pos += 1;
            }
        }
        set_at(&mut pos_true, ih_idx, next_pos);
        next_pos += 1;
        for &lb in loop_blocks {
            if lb > ih && !at(&in_inner, to_idx(lb)) {
                set_at(&mut pos_true, to_idx(lb), next_pos);
                next_pos += 1;
            }
        }

        let mut uses_ok = true;
        for b in 0..blocks {
            let bu = to_u32(b);
            let b_in_loop = at(in_loop, b);
            let b_in_inner = at(&in_inner, b);
            let b_pos = at(&pos_true, b);
            for inst in view.block(Block(bu)) {
                for_each_use(view, inst, |v| {
                    let vi = to_idx(v);
                    let vb = at(inst_block, vi);
                    let vb_idx = to_idx(vb);
                    // Non-param instructions in `ih` must never escape `ih`.
                    if vb == ih && vi >= h_body_start && bu != ih {
                        uses_ok = false;
                        return;
                    }
                    // Values defined in `inner_blocks` may only be used outside `inner_blocks` if
                    // they are `ih_params` used in a post-`ih` block of `loop_blocks`.
                    if !b_in_inner && at(&in_inner, vb_idx) {
                        let is_ih_param = vb == ih && vi >= ih_params.start && vi < ih_params.end;
                        if !(is_ih_param && b_in_loop && bu > ih) {
                            uses_ok = false;
                            return;
                        }
                    }
                    // Verify def-before-use in `L_true` emission order for every instruction in `loop_blocks`.
                    if b_in_loop {
                        let is_ih_param = vb == ih && vi >= ih_params.start && vi < ih_params.end;
                        if !is_ih_param {
                            if at(in_loop, vb_idx) {
                                let v_pos = at(&pos_true, vb_idx);
                                if v_pos == NONE || v_pos > b_pos || (vb == bu && vi >= inst) {
                                    uses_ok = false;
                                }
                            } else if vb > preheader {
                                uses_ok = false;
                            }
                        }
                    }
                });
                if !uses_ok {
                    break;
                }
            }
            if !uses_ok {
                break;
            }
        }

        for &ib in &inner_blocks {
            set_at(&mut in_inner, to_idx(ib), false);
        }
        if !uses_ok {
            continue;
        }

        return Some(UnswitchKind::InnerLoopGuard {
            outer_entry_blk,
            inner_header: ih,
            inner_body,
            inner_exit,
            body_is_then,
            cond_inst,
            inner_blocks,
            init_args,
            exit_args,
        });
    }
    None
}

struct InvariantBrAnalysis<A: Allocator> {
    l_true_blocks: Vec<u32, A>,
    l_false_blocks: Vec<u32, A>,
    reach_out_true: Vec<bool, A>,
    reach_out_false: Vec<bool, A>,
    direct_exit: Option<u32>,
    escaped_vals: Vec<u32, A>,
}

/// Forward BFS from `header` staying inside `in_loop`, folding `br_blk`'s terminator to
/// `take_then` (`true` -> `then`, `false` -> `else`). Returns `(copy_blocks_sorted, in_copy, exit_from_copy)`.
fn collect_copy_reach<A: Allocator + Copy>(
    view: &View<'_>,
    header: u32,
    in_loop: &[bool],
    br_blk: u32,
    br_inst: usize,
    take_then: bool,
    alloc: A,
) -> (Vec<u32, A>, Vec<bool, A>, Vec<bool, A>) {
    let blocks = view.blocks();
    let mut in_copy = filled(blocks, false, &alloc);
    let mut exit_from_copy = filled(blocks, false, &alloc);
    let mut copy_blocks: Vec<u32, A> = Vec::new_in(alloc);
    let mut work: Vec<u32, A> = Vec::new_in(alloc);
    let (folded_target, _, _) = br_arm_target(view, br_inst, take_then);

    set_at(&mut in_copy, to_idx(header), true);
    copy_blocks.push(header);
    work.push(header);

    while let Some(u) = work.pop() {
        if u == br_blk {
            let ti = to_idx(folded_target);
            if at(in_loop, ti) {
                if !at(&in_copy, ti) {
                    set_at(&mut in_copy, ti, true);
                    copy_blocks.push(folded_target);
                    work.push(folded_target);
                }
            } else {
                set_at(&mut exit_from_copy, ti, true);
            }
        } else {
            let term = view.terminator(Block(u));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let ti = to_idx(edge.block);
                    if at(in_loop, ti) {
                        if !at(&in_copy, ti) {
                            set_at(&mut in_copy, ti, true);
                            copy_blocks.push(edge.block);
                            work.push(edge.block);
                        }
                    } else {
                        set_at(&mut exit_from_copy, ti, true);
                    }
                }
            });
        }
    }
    copy_blocks.sort_unstable();
    (copy_blocks, in_copy, exit_from_copy)
}

/// Forward BFS over outside blocks (`!in_loop`) starting from every block in `seeds`.
fn collect_outside_reach<A: Allocator + Copy>(
    view: &View<'_>,
    in_loop: &[bool],
    seeds: &[bool],
    alloc: A,
) -> Vec<bool, A> {
    let blocks = view.blocks();
    let mut reach = filled(blocks, false, &alloc);
    let mut work: Vec<u32, A> = Vec::new_in(alloc);
    for b in 0..blocks {
        if at(seeds, b) && !at(in_loop, b) {
            set_at(&mut reach, b, true);
            work.push(to_u32(b));
        }
    }
    while let Some(u) = work.pop() {
        let term = view.terminator(Block(u));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let ti = to_idx(edge.block);
                if !at(in_loop, ti) && !at(&reach, ti) {
                    set_at(&mut reach, ti, true);
                    work.push(edge.block);
                }
            }
        });
    }
    reach
}

/// Verify every operand used inside `copy_blocks` is already emitted in def-before-use order
/// when `copy_blocks` is emitted in ascending order after `0..=preheader`.
fn check_copy_defs(
    view: &View<'_>,
    preheader: u32,
    in_loop: &[bool],
    inst_block: &[u32],
    copy_blocks: &[u32],
    in_copy: &[bool],
    br_inst: usize,
    take_then: bool,
) -> bool {
    let (_, br_args_start, br_argc) = br_arm_target(view, br_inst, take_then);
    for &lb in copy_blocks {
        for inst in view.block(Block(lb)) {
            let mut ok = true;
            if inst == br_inst {
                for k in 0..br_argc {
                    let v = at(view.extra, br_args_start + k);
                    let vi = to_idx(v);
                    let vb = at(inst_block, vi);
                    let vb_idx = to_idx(vb);
                    if at(in_loop, vb_idx) {
                        if !at(in_copy, vb_idx) || vb > lb || (vb == lb && vi >= inst) {
                            ok = false;
                            break;
                        }
                    } else if vb > preheader {
                        ok = false;
                        break;
                    }
                }
            } else {
                for_each_use(view, inst, |v| {
                    let vi = to_idx(v);
                    let vb = at(inst_block, vi);
                    let vb_idx = to_idx(vb);
                    if at(in_loop, vb_idx) {
                        if !at(in_copy, vb_idx) || vb > lb || (vb == lb && vi >= inst) {
                            ok = false;
                        }
                    } else if vb > preheader {
                        ok = false;
                    }
                });
            }
            if !ok {
                return false;
            }
        }
    }
    true
}

/// Verify every edge from `copy_blocks` to `shared_exit` sees all `shared_escaped_vals`
/// already emitted in `copy_blocks` before that edge's terminator.
fn check_exit_edges_have_escaped_defs(
    view: &View<'_>,
    inst_block: &[u32],
    copy_blocks: &[u32],
    in_copy: &[bool],
    br_blk: u32,
    br_inst: usize,
    take_then: bool,
    shared_exit: u32,
    shared_escaped_vals: &[u32],
) -> bool {
    let (folded_target, _, _) = br_arm_target(view, br_inst, take_then);
    for &lb in copy_blocks {
        let term = view.terminator(Block(lb));
        let jumps_to_exit = if lb == br_blk {
            folded_target == shared_exit
        } else {
            let mut hit = false;
            each(view, term, |item| {
                if let Item::Edge(edge) = item
                    && edge.block == shared_exit
                {
                    hit = true;
                }
            });
            hit
        };
        if jumps_to_exit {
            for &ev in shared_escaped_vals {
                let evi = to_idx(ev);
                let evb = at(inst_block, evi);
                let evb_idx = to_idx(evb);
                if !at(in_copy, evb_idx) || evb > lb || (evb == lb && evi >= term) {
                    return false;
                }
            }
        }
    }
    true
}

/// Per-copy reachability pruning and multi-exit escape analysis for an `InvariantBr` candidate.
fn analyze_invariant_br<A: Allocator + Copy>(
    view: &View<'_>,
    preheader: u32,
    header: u32,
    in_loop: &[bool],
    exit_blocks: &[u32],
    inst_block: &[u32],
    preds: &[Vec<u32, A>],
    br_blk: u32,
    br_inst: usize,
    alloc: A,
) -> Option<InvariantBrAnalysis<A>> {
    let blocks = view.blocks();
    let (l_true_blocks, in_l_true, exit_from_true) =
        collect_copy_reach(view, header, in_loop, br_blk, br_inst, true, alloc);
    let (l_false_blocks, in_l_false, exit_from_false) =
        collect_copy_reach(view, header, in_loop, br_blk, br_inst, false, alloc);

    // Require `br_blk` itself to be reachable from `header` in both copies.
    if !at(&in_l_true, to_idx(br_blk)) || !at(&in_l_false, to_idx(br_blk)) {
        return None;
    }

    // Cloning an `Invoke`/`InvokeIndirect`/`AsmBr` into BOTH copies when its normal/fallthrough
    // target lies outside `in_loop` would violate LIR's single-predecessor rule on that target.
    for &lb in &l_true_blocks {
        if !at(&in_l_false, to_idx(lb)) {
            continue;
        }
        let term = view.terminator(Block(lb));
        if matches!(view.op(term), Op::Invoke | Op::InvokeIndirect | Op::AsmBr) {
            let mut bad_edge = false;
            each(view, term, |item| {
                if let Item::Edge(edge) = item
                    && !at(in_loop, to_idx(edge.block))
                    && !view.block_kind(Block(edge.block)).is_landing()
                {
                    bad_edge = true;
                }
            });
            if bad_edge {
                return None;
            }
        }
    }

    if !check_copy_defs(view, preheader, in_loop, inst_block, &l_true_blocks, &in_l_true, br_inst, true)
        || !check_copy_defs(view, preheader, in_loop, inst_block, &l_false_blocks, &in_l_false, br_inst, false)
    {
        return None;
    }

    let reach_out_true = collect_outside_reach(view, in_loop, &exit_from_true, alloc);
    let reach_out_false = collect_outside_reach(view, in_loop, &exit_from_false, alloc);

    let mut shared_escaped_vals: Vec<u32, A> = Vec::new_in(alloc);
    let mut shared_user_blocks: Vec<u32, A> = Vec::new_in(alloc);
    let mut uses_ok = true;

    for b in 0..blocks {
        if at(in_loop, b) {
            continue;
        }
        let b_u32 = to_u32(b);
        let r_true = at(&reach_out_true, b);
        let r_false = at(&reach_out_false, b);
        for inst in view.block(Block(b_u32)) {
            for_each_use(view, inst, |v| {
                let vi = to_idx(v);
                let vb = at(inst_block, vi);
                let vb_idx = to_idx(vb);
                if at(in_loop, vb_idx) {
                    if b_u32 <= preheader {
                        uses_ok = false;
                        return;
                    }
                    if r_true && !r_false {
                        if !at(&in_l_true, vb_idx) {
                            uses_ok = false;
                        }
                    } else if r_false && !r_true {
                        if !at(&in_l_false, vb_idx) {
                            uses_ok = false;
                        }
                    } else if r_true && r_false {
                        if !at(&in_l_true, vb_idx) || !at(&in_l_false, vb_idx) {
                            uses_ok = false;
                            return;
                        }
                        if shared_escaped_vals.scan_for(&v).is_none() {
                            shared_escaped_vals.push(v);
                        }
                        if shared_user_blocks.scan_for(&b_u32).is_none() {
                            shared_user_blocks.push(b_u32);
                        }
                    } else {
                        // Dead outside block using a loop-defined value: reject so `verify()` never sees `NONE`.
                        uses_ok = false;
                    }
                } else if vb > b_u32 || (vb == b_u32 && vi >= inst) {
                    uses_ok = false;
                }
            });
            if !uses_ok {
                break;
            }
        }
        if !uses_ok {
            break;
        }
    }
    if !uses_ok {
        return None;
    }

    let direct_exit = if shared_escaped_vals.is_empty() {
        None
    } else {
        let mut shared_exit = NONE;
        let mut shared_exit_count = 0u32;
        for &ex in exit_blocks {
            let ex_idx = to_idx(ex);
            if at(&exit_from_true, ex_idx) && at(&exit_from_false, ex_idx) {
                shared_exit = ex;
                shared_exit_count += 1;
            }
        }
        if shared_exit_count != 1
            || shared_exit == NONE
            || shared_exit <= preheader
            || view.block_kind(Block(shared_exit)).is_landing()
        {
            return None;
        }
        let ps = preds.get(to_idx(shared_exit)).map(Vec::as_slice).unwrap_or(&[]);
        if ps.is_empty() {
            return None;
        }
        for &p in ps {
            if !at(in_loop, to_idx(p)) {
                return None;
            }
            let p_term = view.terminator(Block(p));
            let mut edge_ok = true;
            each(view, p_term, |item| {
                if let Item::Edge(edge) = item
                    && edge.block == shared_exit
                    && edge.args.is_none()
                {
                    edge_ok = false;
                }
            });
            if !edge_ok {
                return None;
            }
        }
        for &u in &shared_user_blocks {
            if u < shared_exit {
                return None;
            }
        }
        if !check_exit_edges_have_escaped_defs(
            view,
            inst_block,
            &l_true_blocks,
            &in_l_true,
            br_blk,
            br_inst,
            true,
            shared_exit,
            &shared_escaped_vals,
        ) || !check_exit_edges_have_escaped_defs(
            view,
            inst_block,
            &l_false_blocks,
            &in_l_false,
            br_blk,
            br_inst,
            false,
            shared_exit,
            &shared_escaped_vals,
        ) {
            return None;
        }

        // Prove `shared_exit` dominates every `shared_user_blocks` entry: BFS from
        // `exit_blocks \ {shared_exit}` over `!in_loop` must not reach any shared user block.
        let mut other_seeds = filled(blocks, false, &alloc);
        for &other_ex in exit_blocks {
            if other_ex != shared_exit {
                set_at(&mut other_seeds, to_idx(other_ex), true);
            }
        }
        let reach_other = collect_outside_reach(view, in_loop, &other_seeds, alloc);
        for &u in &shared_user_blocks {
            if at(&reach_other, to_idx(u)) {
                return None;
            }
        }
        Some(shared_exit)
    };

    Some(InvariantBrAnalysis {
        l_true_blocks,
        l_false_blocks,
        reach_out_true,
        reach_out_false,
        direct_exit,
        escaped_vals: shared_escaped_vals,
    })
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

        // Collect all external exit targets of edges leaving `loop_blocks`.
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

        // 1. Scan for an internal `Op::Br` whose condition is defined outside the loop (`<= preheader`),
        //    or is a pure, flag-free `Op::Icmp`/`Op::Fcmp` inside the loop with invariant operands.
        //    Uses per-copy reachability pruning so multi-exit loops (e.g. bounds-check traps) qualify!
        for &bb in &loop_blocks {
            let term = view.terminator(Block(bb));
            if view.op(term) != Op::Br {
                continue;
            }
            let idx = to_idx(at(view.data, term));
            let cv = at(view.extra, idx);
            let cvi = to_idx(cv);
            let cb = at(&inst_block, cvi);
            let candidate = if !at(&in_loop, to_idx(cb)) && cb <= preheader {
                Some((cv, None))
            } else if matches!(view.op(cvi), Op::Icmp | Op::Fcmp) && at(view.flags, cvi) == 0 {
                let cat = to_idx(at(view.data, cvi));
                let lhs = at(view.extra, cat + 1);
                let rhs = at(view.extra, cat + 2);
                if is_outer_invariant(view, lhs, &in_loop, &inst_block, preheader)
                    && is_outer_invariant(view, rhs, &in_loop, &inst_block, preheader)
                {
                    Some((cv, Some(cvi)))
                } else {
                    None
                }
            } else {
                None
            };
            let Some((inv_cond, hoist_cmp)) = candidate else {
                continue;
            };
            if let Some(analysis) = analyze_invariant_br(
                view,
                preheader,
                header,
                &in_loop,
                &exit_blocks,
                &inst_block,
                &preds,
                bb,
                term,
                alloc,
            ) {
                for &lb in &loop_blocks {
                    set_at(&mut in_loop, to_idx(lb), false);
                }
                return Some(UnswitchPlan {
                    preheader,
                    header,
                    loop_blocks,
                    l_true_blocks: analysis.l_true_blocks,
                    l_false_blocks: analysis.l_false_blocks,
                    reach_out_true: analysis.reach_out_true,
                    reach_out_false: analysis.reach_out_false,
                    kind: UnswitchKind::InvariantBr { inv_cond, br_inst: term, hoist_cmp },
                    inst_block,
                    direct_exit: analysis.direct_exit,
                    escaped_vals: analysis.escaped_vals,
                });
            }
        }

        // 2. Otherwise check the single-exit escape invariant and look for a nested inner loop
        //    whose first-iteration header guard `cond(init_args)` is outer-loop-invariant.
        let mut escaped_vals: Vec<u32, A> = Vec::new_in(alloc);
        let mut escaped_user_blocks: Vec<u32, A> = Vec::new_in(alloc);
        for b in 0..blocks {
            if at(&in_loop, b) {
                continue;
            }
            let b_u32 = to_u32(b);
            for inst in view.block(Block(b_u32)) {
                for_each_use(view, inst, |v| {
                    if at(&in_loop, to_idx(at(&inst_block, to_idx(v)))) {
                        if escaped_vals.scan_for(&v).is_none() {
                            escaped_vals.push(v);
                        }
                        if escaped_user_blocks.scan_for(&b_u32).is_none() {
                            escaped_user_blocks.push(b_u32);
                        }
                    }
                });
            }
        }

        let direct_exit = if escaped_vals.is_empty() {
            None
        } else if exit_blocks.len() == 1
            && let Some(&ex) = exit_blocks.first()
        {
            let ps = preds.get(to_idx(ex)).map(Vec::as_slice).unwrap_or(&[]);
            let all_preds_in_loop = !ps.is_empty()
                && ex > preheader
                && !view.block_kind(Block(ex)).is_landing()
                && ps.iter().all(|&p| at(&in_loop, to_idx(p)))
                && escaped_user_blocks.iter().all(|&u| u >= ex);
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

        let Some(kind) = find_inner_loop_guard_candidate(
            view,
            preheader,
            header,
            &loop_blocks,
            &in_loop,
            &inst_block,
            &preds,
            &escaped_vals,
            alloc,
        ) else {
            for &bb in &loop_blocks {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        };

        let mut l_true_blocks: Vec<u32, A> = Vec::with_capacity_in(loop_blocks.len(), alloc);
        let mut l_false_blocks: Vec<u32, A> = Vec::with_capacity_in(loop_blocks.len(), alloc);
        if let UnswitchKind::InnerLoopGuard { inner_header, inner_body, ref inner_blocks, .. } = kind {
            let mut in_inner = filled(blocks, false, &alloc);
            for &ib in inner_blocks {
                set_at(&mut in_inner, to_idx(ib), true);
            }
            for &lb in &loop_blocks {
                if lb < inner_header && !at(&in_inner, to_idx(lb)) {
                    l_true_blocks.push(lb);
                }
            }
            l_true_blocks.push(inner_body);
            for &ib in inner_blocks {
                if ib != inner_header && ib != inner_body {
                    l_true_blocks.push(ib);
                }
            }
            l_true_blocks.push(inner_header);
            for &lb in &loop_blocks {
                if lb > inner_header && !at(&in_inner, to_idx(lb)) {
                    l_true_blocks.push(lb);
                }
            }
            for &lb in &loop_blocks {
                if !at(&in_inner, to_idx(lb)) {
                    l_false_blocks.push(lb);
                }
            }
        }

        for &bb in &loop_blocks {
            set_at(&mut in_loop, to_idx(bb), false);
        }
        return Some(UnswitchPlan {
            preheader,
            header,
            loop_blocks,
            l_true_blocks,
            l_false_blocks,
            reach_out_true: Vec::new_in(alloc),
            reach_out_false: Vec::new_in(alloc),
            kind,
            inst_block,
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

fn emit_jump_with_args<A: Allocator>(
    target: u32,
    args: &[u32],
    val_map: &[u32],
    extra_escaped: &[u32],
    loc: u32,
    func: &mut Func<A>,
) {
    let dst_at = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len() + extra_escaped.len()));
    for &a in args {
        func.extra.push(at(val_map, to_idx(a)));
    }
    for &ev in extra_escaped {
        func.extra.push(at(val_map, to_idx(ev)));
    }
    func.ops.push(op_byte(Op::Jump));
    func.flags.push(0);
    func.tys.push(Ty::NONE.0);
    func.data.push(dst_at);
    func.attrs.push(0);
    func.locs.push(loc);
}

fn emit_inner_header_br<A: Allocator>(
    cond_new: u32,
    body_new: u32,
    body_params: &[u32],
    exit_new: u32,
    exit_args_old: &[u32],
    body_is_then: bool,
    val_map: &[u32],
    extra_escaped: &[u32],
    flags: u8,
    loc: u32,
    func: &mut Func<A>,
) {
    let br_at = to_u32(func.extra.len());
    func.extra.push(cond_new);
    if body_is_then {
        func.extra.push(body_new);
        func.extra.push(to_u32(body_params.len()));
        for &p in body_params {
            func.extra.push(p);
        }
        func.extra.push(exit_new);
        func.extra.push(to_u32(exit_args_old.len() + extra_escaped.len()));
        for &a in exit_args_old {
            func.extra.push(at(val_map, to_idx(a)));
        }
        for &ev in extra_escaped {
            func.extra.push(at(val_map, to_idx(ev)));
        }
    } else {
        func.extra.push(exit_new);
        func.extra.push(to_u32(exit_args_old.len() + extra_escaped.len()));
        for &a in exit_args_old {
            func.extra.push(at(val_map, to_idx(a)));
        }
        for &ev in extra_escaped {
            func.extra.push(at(val_map, to_idx(ev)));
        }
        func.extra.push(body_new);
        func.extra.push(to_u32(body_params.len()));
        for &p in body_params {
            func.extra.push(p);
        }
    }
    func.ops.push(op_byte(Op::Br));
    func.flags.push(flags);
    func.tys.push(Ty::NONE.0);
    func.data.push(br_at);
    func.attrs.push(0);
    func.locs.push(loc);
}

/// Hoists loop-invariant branches (and outer-loop-invariant inner-loop entry guards) out of
/// natural loops (up to `max_loops` unswitches), folding the dead arm out of each specialized
/// clone and running `dead` + `coalesce_cfg`.
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

        // New block layout:
        //   0..=preheader -> unchanged
        //   L_true blocks (immediately after preheader -> hot fast loop!)
        //   L_false blocks (cold fallback loop!)
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
        for &lb in &plan.l_true_blocks {
            set_at(&mut blk_map_true, to_idx(lb), next_blk);
            next_blk += 1;
        }
        for &lb in &plan.l_false_blocks {
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

        // 1. Emit blocks 0..=preheader; rewrite `preheader`'s `Jump` into `Br(cond, L_true, L_false)`.
        for b in 0..=preheader_idx {
            let b_u32 = to_u32(b);
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(v.block_kinds, b));
            let range = v.block(Block(b_u32));
            if b_u32 == plan.preheader {
                let term = range.end.saturating_sub(1);
                for inst in range.start..term {
                    let nv = emit_remapped(&v, inst, &val_map_out, &blk_map_outside, None, &[], &mut func);
                    set_at(&mut val_map_out, inst, nv);
                    set_at(&mut val_map_true, inst, nv);
                    set_at(&mut val_map_false, inst, nv);
                }

                let (hoisted_cond, then_header_new, else_header_new, br_flags) = match &plan.kind {
                    UnswitchKind::InvariantBr { inv_cond, br_inst, hoist_cmp } => {
                        let cond_new = if let Some(cmp_inst) = *hoist_cmp {
                            let mut pre_map = filled(len, NONE, &alloc);
                            for (dst, &src) in pre_map.iter_mut().zip(val_map_out.iter()) {
                                *dst = src;
                            }
                            let cat = to_idx(at(v.data, cmp_inst));
                            for off in [1usize, 2usize] {
                                let op_v = at(v.extra, cat + off);
                                let op_i = to_idx(op_v);
                                if at(&pre_map, op_i) == NONE && is_pure_const(&v, op_i) {
                                    let c_nv = emit_remapped(
                                        &v,
                                        op_i,
                                        &pre_map,
                                        &blk_map_outside,
                                        None,
                                        &[],
                                        &mut func,
                                    );
                                    set_at(&mut pre_map, op_i, c_nv);
                                }
                            }
                            emit_remapped(&v, cmp_inst, &pre_map, &blk_map_outside, None, &[], &mut func)
                        } else {
                            at(&val_map_out, to_idx(*inv_cond))
                        };
                        (
                            cond_new,
                            at(&blk_map_true, to_idx(plan.header)),
                            at(&blk_map_false, to_idx(plan.header)),
                            at(v.flags, *br_inst),
                        )
                    }
                    UnswitchKind::InnerLoopGuard {
                        inner_header,
                        body_is_then,
                        cond_inst,
                        init_args,
                        ..
                    } => {
                        let mut pre_map = filled(len, NONE, &alloc);
                        for (dst, &src) in pre_map.iter_mut().zip(val_map_out.iter()) {
                            *dst = src;
                        }
                        let ih_params = v.params(Block(*inner_header));
                        let h_body_start = ih_params.end;
                        let h_body_end = v.block(Block(*inner_header)).end.saturating_sub(1);
                        for (k, &arg) in init_args.iter().enumerate() {
                            let ai = to_idx(arg);
                            let ab = at(&plan.inst_block, ai);
                            let mapped = if !at(&in_loop, to_idx(ab)) && ab <= plan.preheader {
                                at(&val_map_out, ai)
                            } else if is_pure_const(&v, ai) {
                                let existing = at(&pre_map, ai);
                                if existing != NONE {
                                    existing
                                } else {
                                    let c_nv = emit_remapped(
                                        &v,
                                        ai,
                                        &pre_map,
                                        &blk_map_outside,
                                        None,
                                        &[],
                                        &mut func,
                                    );
                                    set_at(&mut pre_map, ai, c_nv);
                                    c_nv
                                }
                            } else {
                                NONE
                            };
                            if mapped != NONE {
                                set_at(&mut pre_map, ih_params.start + k, mapped);
                            }
                        }
                        for hi in h_body_start..h_body_end {
                            for_each_use(&v, hi, |u| {
                                let ui = to_idx(u);
                                if at(&pre_map, ui) == NONE && is_pure_const(&v, ui) {
                                    let c_nv = emit_remapped(
                                        &v,
                                        ui,
                                        &pre_map,
                                        &blk_map_outside,
                                        None,
                                        &[],
                                        &mut func,
                                    );
                                    set_at(&mut pre_map, ui, c_nv);
                                }
                            });
                        }
                        for hi in h_body_start..h_body_end {
                            let nv = emit_remapped(&v, hi, &pre_map, &blk_map_outside, None, &[], &mut func);
                            set_at(&mut pre_map, hi, nv);
                        }
                        let cond_new = at(&pre_map, to_idx(*cond_inst));
                        let (then_hdr, else_hdr) = if *body_is_then {
                            (at(&blk_map_true, to_idx(plan.header)), at(&blk_map_false, to_idx(plan.header)))
                        } else {
                            (at(&blk_map_false, to_idx(plan.header)), at(&blk_map_true, to_idx(plan.header)))
                        };
                        (cond_new, then_hdr, else_hdr, 0u8)
                    }
                };

                let jat = to_idx(at(v.data, term));
                let argc_u32 = at(v.extra, jat + 1);
                let argc = to_idx(argc_u32);
                let br_at = to_u32(func.extra.len());
                func.extra.push(hoisted_cond);
                func.extra.push(then_header_new);
                func.extra.push(argc_u32);
                for a in 0..argc {
                    func.extra.push(at(&val_map_out, to_idx(at(v.extra, jat + 2 + a))));
                }
                func.extra.push(else_header_new);
                func.extra.push(argc_u32);
                for a in 0..argc {
                    func.extra.push(at(&val_map_out, to_idx(at(v.extra, jat + 2 + a))));
                }
                func.ops.push(op_byte(Op::Br));
                func.flags.push(br_flags);
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

        // 2. Emit `L_true` clone.
        match &plan.kind {
            UnswitchKind::InvariantBr { br_inst, .. } => {
                for &lb in &plan.l_true_blocks {
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
                    let range = v.block(Block(lb));
                    for inst in range {
                        if inst == *br_inst {
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
            }
            UnswitchKind::InnerLoopGuard {
                outer_entry_blk,
                inner_header,
                inner_body,
                inner_exit,
                body_is_then,
                cond_inst,
                init_args,
                exit_args,
                ..
            } => {
                let ih_params = v.params(Block(*inner_header));
                for &lb in &plan.l_true_blocks {
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
                    let range = v.block(Block(lb));
                    let term = range.end.saturating_sub(1);
                    if lb == *outer_entry_blk {
                        for inst in range.start..term {
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
                        // Jump directly into `inner_body_true(init_args)`, skipping the first guard!
                        emit_jump_with_args(
                            at(&blk_map_true, to_idx(*inner_body)),
                            init_args.as_slice(),
                            &val_map_true,
                            &[],
                            at(v.locs, term),
                            &mut func,
                        );
                    } else if lb == *inner_body {
                        // Materialize `inner_header`'s params directly at the top of `inner_body_true`.
                        for k in 0..ih_params.len() {
                            let hp = ih_params.start + k;
                            let p_idx = to_u32(func.ops.len());
                            func.ops.push(op_byte(Op::Param));
                            func.flags.push(0);
                            func.tys.push(at(v.tys, hp));
                            func.data.push(0);
                            func.attrs.push(0);
                            func.locs.push(at(v.locs, hp));
                            set_at(&mut val_map_true, hp, p_idx);
                        }
                        for inst in range {
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
                    } else if lb == *inner_header {
                        // Rotated bottom latch check: emit fresh params, guard body, and `Br`
                        // passing those params back to `inner_body_true` (or exiting to `inner_exit_true`).
                        let mut ih_true_params: Vec<u32, A> = Vec::with_capacity_in(ih_params.len(), alloc);
                        for k in 0..ih_params.len() {
                            let hp = ih_params.start + k;
                            let p_idx = to_u32(func.ops.len());
                            func.ops.push(op_byte(Op::Param));
                            func.flags.push(0);
                            func.tys.push(at(v.tys, hp));
                            func.data.push(0);
                            func.attrs.push(0);
                            func.locs.push(at(v.locs, hp));
                            set_at(&mut val_map_true, hp, p_idx);
                            ih_true_params.push(p_idx);
                        }
                        for inst in ih_params.end..term {
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
                        let extra_esc: &[u32] = if plan.direct_exit == Some(*inner_exit) {
                            plan.escaped_vals.as_slice()
                        } else {
                            &[]
                        };
                        emit_inner_header_br(
                            at(&val_map_true, to_idx(*cond_inst)),
                            at(&blk_map_true, to_idx(*inner_body)),
                            ih_true_params.as_slice(),
                            at(&blk_map_true, to_idx(*inner_exit)),
                            exit_args.as_slice(),
                            *body_is_then,
                            &val_map_true,
                            extra_esc,
                            at(v.flags, term),
                            at(v.locs, term),
                            &mut func,
                        );
                    } else {
                        for inst in range {
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
            }
        }

        // 3. Emit `L_false` clone.
        match &plan.kind {
            UnswitchKind::InvariantBr { br_inst, .. } => {
                for &lb in &plan.l_false_blocks {
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
                    let range = v.block(Block(lb));
                    for inst in range {
                        if inst == *br_inst {
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
            }
            UnswitchKind::InnerLoopGuard {
                outer_entry_blk,
                inner_header,
                inner_exit,
                init_args,
                exit_args,
                ..
            } => {
                let ih_params = v.params(Block(*inner_header));
                for &lb in &plan.l_false_blocks {
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
                    let range = v.block(Block(lb));
                    let term = range.end.saturating_sub(1);
                    if lb == *outer_entry_blk {
                        for inst in range.start..term {
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
                        // Substitute `ih_params` with mapped `init_args`, then jump straight to `inner_exit_false`!
                        for (k, &arg) in init_args.iter().enumerate() {
                            let mapped_arg = at(&val_map_false, to_idx(arg));
                            set_at(&mut val_map_false, ih_params.start + k, mapped_arg);
                        }
                        let extra_esc: &[u32] = if plan.direct_exit == Some(*inner_exit) {
                            plan.escaped_vals.as_slice()
                        } else {
                            &[]
                        };
                        emit_jump_with_args(
                            at(&blk_map_false, to_idx(*inner_exit)),
                            exit_args.as_slice(),
                            &val_map_false,
                            extra_esc,
                            at(v.locs, term),
                            &mut func,
                        );
                    } else {
                        for inst in range {
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
            }
        }

        // 4. Emit remaining non-loop blocks after `preheader` (inserting promoted params on `direct_exit`
        //    and binding true-only/false-only exit regions directly to their dominating copy's values!).
        let mut val_map_tmp: Vec<u32, A> = Vec::new_in(alloc);
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
            let true_only = at(&plan.reach_out_true, b) && !at(&plan.reach_out_false, b);
            let false_only = at(&plan.reach_out_false, b) && !at(&plan.reach_out_true, b);
            if true_only || false_only {
                if val_map_tmp.len() != len {
                    val_map_tmp = filled(len, NONE, &alloc);
                }
                for (dst, &src) in val_map_tmp.iter_mut().zip(val_map_out.iter()) {
                    *dst = src;
                }
                let (copy_blocks, copy_vals) = if true_only {
                    (plan.l_true_blocks.as_slice(), val_map_true.as_slice())
                } else {
                    (plan.l_false_blocks.as_slice(), val_map_false.as_slice())
                };
                for &lb in copy_blocks {
                    for li in v.block(Block(lb)) {
                        set_at(&mut val_map_tmp, li, at(copy_vals, li));
                    }
                }
                for inst in params.end..range.end {
                    let nv = emit_remapped(&v, inst, &val_map_tmp, &blk_map_outside, None, &[], &mut func);
                    set_at(&mut val_map_out, inst, nv);
                    set_at(&mut val_map_tmp, inst, nv);
                }
            } else {
                for inst in params.end..range.end {
                    let nv = emit_remapped(&v, inst, &val_map_out, &blk_map_outside, None, &[], &mut func);
                    set_at(&mut val_map_out, inst, nv);
                }
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
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{Conv, SlotKind},
        ifconv::if_convert,
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

    #[test]
    fn unswitch_hoists_invariant_inner_guard_out_of_bsearch_batch() {
        // Exact nested `bsearch_batch` shape:
        //   entry -> h_q(q, sum) -> b_q -> h_bs(lo, hi) -> b_bs -> {lt_blk, ge_blk} -> h_bs
        //   h_bs -> end_bs(found) -> h_q
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let arr_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let keys_slot = b.slot(8 * 8, 3, SlotKind::Temp);
        let [entry, h_q, b_q, h_bs, b_bs, lt_blk, ge_blk, end_bs, exit] =
            [(); 9].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let m = b.param(Ty::I64);
        let arr = b.slot_addr(arr_slot);
        let keys = b.slot_addr(keys_slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k * 7 + 3));
            b.store(Flags::NONE, v, arr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let query_vals = [0u64, 3, 10, 24, 52, 73, 108, 999];
        for (k, &qv) in query_vals.iter().enumerate() {
            let v = b.iconst(Ty::I64, u128::from(qv));
            b.store(Flags::NONE, v, keys, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        b.jump((h_q, &[zero, zero]));

        b.begin(h_q);
        let q = b.param(Ty::I64);
        let sum = b.param(Ty::I64);
        let kq = b.icmp(IntCond::Ult, q, m);
        b.br(Flags::NONE, kq, (b_q, &[]), (exit, &[sum]));

        b.begin(b_q);
        let q_off = b.binary(Op::Shl, Flags::NONE, q, three);
        let kp = b.ptr_add(Flags::NONE, keys, q_off);
        let key = b.load(Ty::I64, Flags::NONE, kp, 0, MemAttrs::aligned(3));
        b.jump((h_bs, &[zero, n]));

        b.begin(h_bs);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let kbs = b.icmp(IntCond::Ult, lo, hi);
        b.br(Flags::NONE, kbs, (b_bs, &[]), (end_bs, &[lo]));

        b.begin(b_bs);
        let diff = b.binary(Op::Sub, Flags::NONE, hi, lo);
        let half = b.binary(Op::Lshr, Flags::NONE, diff, one);
        let mid = b.binary(Op::Add, Flags::NONE, lo, half);
        let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let ap = b.ptr_add(Flags::NONE, arr, m_off);
        let mv = b.load(Ty::I64, Flags::NONE, ap, 0, MemAttrs::aligned(3));
        let is_lt = b.icmp(IntCond::Ult, mv, key);
        b.br(Flags::NONE, is_lt, (lt_blk, &[]), (ge_blk, &[]));

        b.begin(lt_blk);
        let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
        b.jump((h_bs, &[mid1, hi]));

        b.begin(ge_blk);
        b.jump((h_bs, &[lo, mid]));

        b.begin(end_bs);
        let found = b.param(Ty::I64);
        let sum2 = b.binary(Op::Add, Flags::NONE, sum, found);
        let q2 = b.binary(Op::Add, Flags::NONE, q, one);
        b.jump((h_q, &[q2, sum2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unswitch_loops(func.view(), 2, TestHeap).unwrap();
        assert!(out.stats.loops_unswitched >= 1);
        assert!(verify(out.func.view()).is_ok());

        // Entry now branches on hoisted `0 < n` (`Op::Br`), and after `if_convert`, the hot
        // `b_q_true` jumps directly into a single-block do-while `b_bs` with zero `0 < n` guard!
        assert_eq!(out.func.view().op(out.func.view().terminator(Block(0))), Op::Br);
        let ifc = if_convert(out.func.view(), 8, TestHeap).unwrap();
        assert!(verify(ifc.func.view()).is_ok());
        assert!(ifc.stats.branches_converted >= 1);

        // Verify evaluator equivalence for `n = 0`, `n = 1`, `n = 16` across multiple query counts!
        for &nv in &[0u128, 1, 16] {
            for &mv in &[0u128, 1, 4, 8] {
                let args = [Val::Int(nv), Val::Int(mv)];
                let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("orig bsearch eval failed");
                };
                let Outcome::Returned(unsw) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("unswitched bsearch eval failed");
                };
                let Outcome::Returned(after_ifc) = eval::run_in(ifc.func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("if_convert bsearch eval failed");
                };
                assert_eq!(orig, unsw, "unswitch bsearch mismatch for n={nv}, m={mv}");
                assert_eq!(orig, after_ifc, "post-ifc bsearch mismatch for n={nv}, m={mv}");
            }
        }
    }

    #[test]
    fn unswitch_hoists_invariant_inner_guard_two_block_inverted_polarity() {
        // 2-block inner loop whose header tests `i >= n` (exit on `then`, body on `else`):
        // `for q in 0..m { for i in 0..n { sum += q + i; } }`
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, h_q, b_q, h_i, b_i, end_i, exit] = [(); 7].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let m = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        b.jump((h_q, &[zero, zero]));

        b.begin(h_q);
        let q = b.param(Ty::I64);
        let sum = b.param(Ty::I64);
        let kq = b.icmp(IntCond::Ult, q, m);
        b.br(Flags::NONE, kq, (b_q, &[]), (exit, &[sum]));

        b.begin(b_q);
        b.jump((h_i, &[zero, sum]));

        b.begin(h_i);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Uge, i, n);
        b.br(Flags::NONE, done, (end_i, &[acc]), (b_i, &[]));

        b.begin(b_i);
        let qi = b.binary(Op::Add, Flags::NONE, q, i);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, qi);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((h_i, &[i2, acc2]));

        b.begin(end_i);
        let res_acc = b.param(Ty::I64);
        let q2 = b.binary(Op::Add, Flags::NONE, q, one);
        b.jump((h_q, &[q2, res_acc]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unswitch_loops(func.view(), 2, TestHeap).unwrap();
        assert_eq!(out.stats.loops_unswitched, 1);
        assert!(verify(out.func.view()).is_ok());

        for &nv in &[0u128, 1, 7, 16] {
            for &mv in &[0u128, 1, 5] {
                let args = [Val::Int(nv), Val::Int(mv)];
                let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("orig eval failed");
                };
                let Outcome::Returned(unsw) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("unsw eval failed");
                };
                assert_eq!(orig, unsw, "inverted inner-guard mismatch for n={nv}, m={mv}");
            }
        }
    }

    #[test]
    fn unswitch_hoists_loop_local_invariant_icmp_branch() {
        // Post-rotation outer loop whose body contains `c = icmp(Ult, 0, n); br(c, do_while, end_in)`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, h_q, b_q, do_in, end_in, exit] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let m = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        b.jump((h_q, &[zero, zero]));

        b.begin(h_q);
        let q = b.param(Ty::I64);
        let sum = b.param(Ty::I64);
        let kq = b.icmp(IntCond::Ult, q, m);
        b.br(Flags::NONE, kq, (b_q, &[]), (exit, &[sum]));

        b.begin(b_q);
        let has_in = b.icmp(IntCond::Ult, zero, n);
        b.br(Flags::NONE, has_in, (do_in, &[zero, sum]), (end_in, &[sum]));

        b.begin(do_in);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, i);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (do_in, &[i2, acc2]), (end_in, &[acc2]));

        b.begin(end_in);
        let res_acc = b.param(Ty::I64);
        let q2 = b.binary(Op::Add, Flags::NONE, q, one);
        b.jump((h_q, &[q2, res_acc]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = unswitch_loops(func.view(), 2, TestHeap).unwrap();
        assert_eq!(out.stats.loops_unswitched, 1);
        assert!(verify(out.func.view()).is_ok());

        for &nv in &[0u128, 1, 16] {
            for &mv in &[0u128, 1, 5] {
                let args = [Val::Int(nv), Val::Int(mv)];
                let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("orig eval failed");
                };
                let Outcome::Returned(unsw) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("unsw eval failed");
                };
                assert_eq!(orig, unsw, "loop-local icmp unswitch mismatch for n={nv}, m={mv}");
            }
        }
    }

    #[test]
    fn unswitch_hoists_invariant_guard_from_multi_exit_nested_probe_loop() {
        // Exact `hash_probe_batch` CFG: multi-exit outer+inner probe loop where
        // - `ret_blk` is shared (reads escaped `sum` directly, no edge arg),
        // - `panic_keys` is false-only (reads escaped `slot` directly, then traps),
        // - `panic_vals` is true-only (reads escaped `slot` directly, then traps).
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let keys_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let vals_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let queries_slot = b.slot(8 * 8, 3, SlotKind::Temp);
        let err_slot = b.slot(8, 3, SlotKind::Temp);

        let [
            entry,
            outer_hdr,
            probe_init,
            inner_hdr,
            probe,
            hit_check,
            hit_add,
            miss_check,
            miss_step,
            ret_blk,
            panic_keys,
            panic_vals,
        ] = [(); 12].map(|_| b.create_block(BlockKind::Normal));

        const HASH_C: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut table_keys = [0u64; 16];
        let mut table_vals = [0u64; 16];
        let base_k = 101u64;
        let base_bucket = (base_k.wrapping_mul(HASH_C) >> 44) & 15;
        let mut colliding_k = 0u64;
        for cand in 200..2000u64 {
            if ((cand.wrapping_mul(HASH_C) >> 44) & 15) == base_bucket {
                colliding_k = cand;
                break;
            }
        }
        assert_ne!(colliding_k, 0, "must find a colliding key for 16 buckets");
        let seed_keys = [base_k, colliding_k, 303, 404, 505, 606];
        for (idx, &k) in seed_keys.iter().enumerate() {
            let mut s = (k.wrapping_mul(HASH_C) >> 44) & 15;
            while table_keys[s as usize] != 0 {
                s = (s + 1) & 15;
            }
            table_keys[s as usize] = k;
            table_vals[s as usize] = (idx as u64 + 1) * 111 + 7;
        }
        let queries = [base_k, colliding_k, 999_999, 303, 888_888, 505, 777_777, 606];

        b.begin(entry);
        let keys_len = b.param(Ty::I64);
        let vals_len = b.param(Ty::I64);
        let queries_len = b.param(Ty::I64);
        let keys_ptr = b.slot_addr(keys_slot);
        let vals_ptr = b.slot_addr(vals_slot);
        let queries_ptr = b.slot_addr(queries_slot);
        let err_ptr = b.slot_addr(err_slot);
        for (idx, (&tk, &tv)) in table_keys.iter().zip(table_vals.iter()).enumerate() {
            let kv = b.iconst(Ty::I64, u128::from(tk));
            let vv = b.iconst(Ty::I64, u128::from(tv));
            let off = (idx * 8) as i32;
            b.store(Flags::NONE, kv, keys_ptr, off, MemAttrs::aligned(3));
            b.store(Flags::NONE, vv, vals_ptr, off, MemAttrs::aligned(3));
        }
        for (idx, &qv) in queries.iter().enumerate() {
            let q_inst = b.iconst(Ty::I64, u128::from(qv));
            b.store(Flags::NONE, q_inst, queries_ptr, (idx * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let eight = b.iconst(Ty::I64, 8);
        let shift44 = b.iconst(Ty::I64, 44);
        let hash_c = b.iconst(Ty::I64, u128::from(HASH_C));
        let q_bytes = b.binary(Op::Shl, Flags::NONE, queries_len, three);
        let end = b.ptr_add(Flags::NONE, queries_ptr, q_bytes);
        let mask = b.binary(Op::Sub, Flags::NONE, keys_len, one);
        b.jump((outer_hdr, &[queries_ptr, zero]));

        b.begin(outer_hdr);
        let ptr = b.param(Ty::PTR);
        let sum = b.param(Ty::I64);
        let done = b.icmp(IntCond::Eq, ptr, end);
        b.br(Flags::NONE, done, (ret_blk, &[]), (probe_init, &[]));

        b.begin(probe_init);
        let q = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(3));
        let h1 = b.binary(Op::Mul, Flags::NONE, q, hash_c);
        let h2 = b.binary(Op::Lshr, Flags::NONE, h1, shift44);
        let slot0 = b.binary(Op::And, Flags::NONE, h2, mask);
        b.jump((inner_hdr, &[slot0]));

        b.begin(inner_hdr);
        let slot = b.param(Ty::I64);
        let has_keys = b.icmp(IntCond::Ne, keys_len, zero);
        b.br(Flags::NONE, has_keys, (probe, &[]), (panic_keys, &[]));

        b.begin(probe);
        let k_off = b.binary(Op::Shl, Flags::NONE, slot, three);
        let kp = b.ptr_add(Flags::NONE, keys_ptr, k_off);
        let k = b.load(Ty::I64, Flags::NONE, kp, 0, MemAttrs::aligned(3));
        let is_hit = b.icmp(IntCond::Eq, k, q);
        b.br(Flags::NONE, is_hit, (hit_check, &[]), (miss_check, &[]));

        b.begin(hit_check);
        let in_vals = b.icmp(IntCond::Ult, slot, vals_len);
        b.br(Flags::NONE, in_vals, (hit_add, &[]), (panic_vals, &[]));

        b.begin(hit_add);
        let v_off = b.binary(Op::Shl, Flags::NONE, slot, three);
        let vp = b.ptr_add(Flags::NONE, vals_ptr, v_off);
        let val = b.load(Ty::I64, Flags::NONE, vp, 0, MemAttrs::aligned(3));
        let sum2 = b.binary(Op::Add, Flags::NONE, sum, val);
        let ptr2 = b.ptr_add(Flags::NONE, ptr, eight);
        b.jump((outer_hdr, &[ptr2, sum2]));

        b.begin(miss_check);
        let is_empty = b.icmp(IntCond::Eq, k, zero);
        let ptr_miss = b.ptr_add(Flags::NONE, ptr, eight);
        b.br(Flags::NONE, is_empty, (outer_hdr, &[ptr_miss, sum]), (miss_step, &[]));

        b.begin(miss_step);
        let slot_inc = b.binary(Op::Add, Flags::NONE, slot, one);
        let slot2 = b.binary(Op::And, Flags::NONE, slot_inc, mask);
        b.jump((inner_hdr, &[slot2]));

        b.begin(ret_blk);
        b.ret(&[sum]);

        b.begin(panic_keys);
        b.store(Flags::NONE, slot, err_ptr, 0, MemAttrs::aligned(3));
        b.trap(1);

        b.begin(panic_vals);
        b.store(Flags::NONE, slot, err_ptr, 0, MemAttrs::aligned(3));
        b.trap(2);

        let func = b.finish_checked().unwrap();

        let out = unswitch_loops(func.view(), 2, alloc).unwrap();
        assert!(out.stats.loops_unswitched >= 1, "expected multi-exit probe loop to unswitch");
        assert!(verify(out.func.view()).is_ok());

        // Entry now branches on hoisted `Ne(keys_len, 0)` and NO `keys_len` comparison remains
        // anywhere inside the unswitched fast loop!
        let ov = out.func.view();
        assert_eq!(ov.op(ov.terminator(Block(0))), Op::Br);
        for blk in 1..ov.blocks() {
            for inst in ov.block(Block(to_u32(blk))) {
                if ov.op(inst) == Op::Icmp {
                    let cat = to_idx(at(ov.data, inst));
                    let lhs = at(ov.extra, cat + 1);
                    let rhs = at(ov.extra, cat + 2);
                    assert!(
                        lhs != 0 && rhs != 0,
                        "keys_len guard must not remain inside unswitched loop (block {blk}, inst {inst})"
                    );
                }
            }
        }

        // 1. `keys_len == 0, queries_len == 0` -> returns `0` (must NOT trap!).
        {
            let args = [Val::Int(0), Val::Int(0), Val::Int(0)];
            let orig = eval::run_in(func.view(), &args, 50_000, alloc).unwrap();
            let unsw = eval::run_in(ov, &args, 50_000, alloc).unwrap();
            assert_eq!(orig, unsw);
            assert!(matches!(unsw, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(0)]));
        }

        // 2. `keys_len == 0, queries_len == 2` -> traps identically to original!
        {
            let args = [Val::Int(0), Val::Int(0), Val::Int(2)];
            let orig = eval::run_in(func.view(), &args, 50_000, alloc).unwrap();
            let unsw = eval::run_in(ov, &args, 50_000, alloc).unwrap();
            assert!(matches!(orig, Outcome::Trapped(_)));
            assert_eq!(orig, unsw);
        }

        // 3. `keys_len == 16, vals_len == 16`, mix of hits, misses, and linear-probe collisions -> identical `sum`!
        {
            let args = [Val::Int(16), Val::Int(16), Val::Int(8)];
            let orig = eval::run_in(func.view(), &args, 50_000, alloc).unwrap();
            let unsw = eval::run_in(ov, &args, 50_000, alloc).unwrap();
            assert!(matches!(orig, Outcome::Returned(_)));
            assert_eq!(orig, unsw);
        }

        // 4. `keys_len == 16, vals_len == 0, queries_len == 8` -> traps via true-only `panic_vals` identically!
        {
            let args = [Val::Int(16), Val::Int(0), Val::Int(8)];
            let orig = eval::run_in(func.view(), &args, 50_000, alloc).unwrap();
            let unsw = eval::run_in(ov, &args, 50_000, alloc).unwrap();
            assert!(matches!(orig, Outcome::Trapped(_)));
            assert_eq!(orig, unsw);
        }
    }

    #[test]
    fn unswitch_rejects_shared_escape_with_two_shared_exits() {
        // Soundness negative test: `acc` (defined in `header`) escapes directly without exit params
        // to TWO distinct shared exits (`exit_a` and `exit_b`), both reachable from `L_true` and `L_false`.
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I1), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, fast, slow, check_limit, latch, exit_a, exit_b] =
            [(); 9].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let mode = b.param(Ty::I1);
        let limit = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let five = b.iconst(Ty::I64, 5);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Uge, i, n);
        b.br(Flags::NONE, done, (exit_a, &[]), (body, &[]));

        b.begin(body);
        b.br(Flags::NONE, mode, (fast, &[]), (slow, &[]));

        b.begin(fast);
        let acc_f = b.binary(Op::Add, Flags::NONE, acc, three);
        b.jump((check_limit, &[acc_f]));

        b.begin(slow);
        let acc_s = b.binary(Op::Add, Flags::NONE, acc, five);
        b.jump((check_limit, &[acc_s]));

        b.begin(check_limit);
        let acc2 = b.param(Ty::I64);
        let over = b.icmp(IntCond::Uge, acc2, limit);
        b.br(Flags::NONE, over, (exit_b, &[]), (latch, &[]));

        b.begin(latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit_a);
        b.ret(&[acc]);

        b.begin(exit_b);
        b.ret(&[acc]);

        let func = b.finish_checked().unwrap();

        let out = unswitch_loops(func.view(), 2, alloc).unwrap();
        assert_eq!(out.stats.loops_unswitched, 0, "must reject loop with shared value escaping via two shared exits");
        assert!(verify(out.func.view()).is_ok());

        for &m in &[0u128, 1u128] {
            for &lim in &[7u128, 100u128] {
                let args = [Val::Int(5), Val::Int(m), Val::Int(lim)];
                let orig = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
                let unsw = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
                assert_eq!(orig, unsw);
            }
        }
    }
}

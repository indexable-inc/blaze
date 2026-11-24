//! AArch64 release-mode optimization helpers for `TierName::Sota`:
//! - **Granlund–Montgomery magic-multiply unsigned division** (replaces 12-cycle `udiv` with
//!   3-cycle `umulh` + shift for arbitrary non-power-of-2 64-bit constant divisors)
//! - **PGO/Cold block layout ordering** (sinks cold/trap/unreachable blocks after hot blocks so
//!   hot paths fall through straight-line in I-cache)
//! - **Tail-Call Optimization (`TCO`) site detection** (`r = call f(args); return r` -> `b f`)
//! - **Dense switch jump-table planner** (O(1) PC-relative `ldrsw` + `br` dispatch).

use blaze_lir::{
    Block, BlockKind, Op, Ty, View,
    func::{Conv, ParamKind, SigId},
};
use blaze_own::{Alloc, Vec};

#[inline]
fn idx(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Plan for replacing 64-bit unsigned division `n / d` (`d >= 3`, non-power-of-2) with `umulh`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MagicUdiv64 {
    /// `d > u64::MAX / 2`: `n / d == u64::from(n >= d)` (single `cmp + cset hs`!).
    CompareGe(u64),
    /// Direct multiply-high: `umulh(n, magic) >> shift`.
    MulShift { magic: u64, shift: u32 },
    /// Overflowing-magic form: `q = umulh(n, magic); (((n - q) >> 1) + q) >> post_shift`.
    MulAddShift { magic: u64, post_shift: u32 },
}

/// Computes the exact Granlund–Montgomery magic multiplier for 64-bit unsigned division by `d`.
///
/// Returns `None` for `d == 0`, `d == 1`, or powers of 2 (which strength-reduce to `lsr`).
/// Exact for every `n` in `0..=u64::MAX`.
pub(crate) fn compute_magic_udiv64(d: u64) -> Option<MagicUdiv64> {
    if d <= 2 || d.is_power_of_two() {
        return None;
    }
    if d > (u64::MAX / 2) {
        return Some(MagicUdiv64::CompareGe(d));
    }
    let d128 = u128::from(d);
    let max_n = u128::from(u64::MAX);
    // Largest n <= 2^64 - 1 with n ≡ -1 (mod d): nc = floor(2^64 / d) * d - 1.
    let two64 = 1u128 << 64;
    let nc = two64 - (two64 % d128) - 1;

    // 1. Try direct `umulh(n, m) >> shift` (`m < 2^64`).
    for shift in 0..63u32 {
        let two_p = 1u128 << (64 + shift);
        let rem = two_p % d128;
        let delta = d128 - rem;
        let m = (two_p + delta) / d128;
        if m <= max_n && nc.checked_mul(delta).is_some_and(|p| p < two_p) {
            if let Ok(magic) = u64::try_from(m) {
                return Some(MagicUdiv64::MulShift { magic, shift });
            }
        }
    }

    // 2. Overflowing-magic form (`2^64 <= m < 2^65`, `shift >= 1`).
    for shift in 1..64u32 {
        let two_p = 1u128 << (64 + shift);
        let rem = two_p % d128;
        let delta = d128 - rem;
        let m = (two_p + delta) / d128;
        if m > max_n && m < (two64 << 1) && nc.checked_mul(delta).is_some_and(|p| p < two_p) {
            if let Ok(magic) = u64::try_from(m - two64) {
                return Some(MagicUdiv64::MulAddShift {
                    magic,
                    post_shift: shift - 1,
                });
            }
        }
    }
    None
}

/// Returns `true` when block `b` is cold (explicitly marked `BlockKind::Cold`, or terminates in
/// `Op::Trap` / `Op::Unreachable` with no side-effecting return).
#[inline]
pub(crate) fn is_cold_block(view: &View<'_>, b: usize) -> bool {
    if b == 0 {
        return false;
    }
    let kind = view.block_kind(Block(u32_of(b)));
    if kind == BlockKind::Cold || kind.is_landing() {
        return true;
    }
    let term = view.terminator(Block(u32_of(b)));
    matches!(view.op(term), Op::Trap | Op::Unreachable | Op::Resume)
}

/// Maximum block count for exact natural-loop depth calculation; mega-functions above this
/// threshold fall back to the strictly O(B) block-ID interval difference estimate.
const EXACT_LOOP_DEPTH_MAX_BLOCKS: usize = 4096;

/// Visits every valid CFG successor block index of block `b`.
fn for_each_succ(view: &View<'_>, b: usize, mut f: impl FnMut(usize)) {
    let blocks = view.blocks();
    let term = view.terminator(Block(u32_of(b)));
    let mut ops = view.operands(term);
    let mut emit = |blk: Block| {
        let t = idx(blk.0);
        if t < blocks {
            f(t);
        }
    };
    match view.op(term) {
        Op::Jump => emit(ops.target().block),
        Op::Br => {
            let _ = ops.value();
            emit(ops.target().block);
            emit(ops.target().block);
        }
        Op::Switch => {
            let scrut = ops.value();
            emit(ops.target().block);
            for (_, tgt) in ops.cases(scrut) {
                emit(tgt.block);
            }
        }
        Op::Invoke => {
            let _ = ops.reference();
            let _ = ops.sig();
            emit(ops.block());
            emit(ops.block());
        }
        Op::InvokeIndirect => {
            let _ = ops.value();
            let _ = ops.sig();
            emit(ops.block());
            emit(ops.block());
        }
        Op::AsmBr => {
            let _ = ops.asm();
            emit(ops.block());
            for &lbl in ops.blocks() {
                emit(Block(lbl));
            }
        }
        Op::Suspend => {
            let _ = ops.imm();
            emit(ops.block());
            emit(ops.block());
        }
        _ => {}
    }
}

/// Computes exact natural-loop nesting depth per block using a flat CSR predecessor graph
/// and per-header backward BFS.
///
/// Grouping all latches of a single header `hdr` into one BFS ensures multi-latch loops
/// increment each member's depth once (not once per latch), and exit blocks whose block ID
/// falls between `hdr` and an inner latch do not inherit inner-loop depth.
fn compute_exact_loop_depths<'a>(
    view: &View<'_>,
    is_ret: &[bool],
    alloc: Alloc<'a>,
) -> Vec<i32, Alloc<'a>> {
    let blocks = view.blocks();
    let mut pred_counts: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut latch_head: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        pred_counts.push(0);
        latch_head.push(usize::MAX);
    }
    let mut latch_src: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut latch_next: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut total_edges: usize = 0;

    for b in 0..blocks {
        for_each_succ(view, b, |succ| {
            if let Some(cnt) = pred_counts.get_mut(succ) {
                *cnt += 1;
                total_edges += 1;
            }
            if succ <= b && !is_ret.get(succ).copied().unwrap_or(false) {
                if let Some(head) = latch_head.get_mut(succ) {
                    let node = latch_src.len();
                    latch_src.push(b);
                    latch_next.push(*head);
                    *head = node;
                }
            }
        });
    }

    let mut pred_starts: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks + 1, alloc);
    let mut pred_cursor: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut offset: usize = 0;
    for b in 0..blocks {
        pred_starts.push(offset);
        pred_cursor.push(offset);
        offset += pred_counts.get(b).copied().unwrap_or(0);
    }
    pred_starts.push(offset);

    let mut pred_list: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(total_edges, alloc);
    for _ in 0..total_edges {
        pred_list.push(0);
    }
    for b in 0..blocks {
        for_each_succ(view, b, |succ| {
            if let Some(cur) = pred_cursor.get_mut(succ) {
                if let Some(slot) = pred_list.get_mut(*cur) {
                    *slot = b;
                    *cur += 1;
                }
            }
        });
    }

    let mut depth: Vec<i32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut stamp: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        depth.push(0);
        stamp.push(0);
    }
    let mut work: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut loop_members: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut cur_stamp: u32 = 0;

    for hdr in 0..blocks {
        let mut node = latch_head.get(hdr).copied().unwrap_or(usize::MAX);
        if node == usize::MAX {
            continue;
        }
        cur_stamp = cur_stamp.wrapping_add(1);
        work.clear();
        loop_members.clear();

        if let Some(s) = stamp.get_mut(hdr) {
            *s = cur_stamp;
        }
        loop_members.push(hdr);

        while node != usize::MAX {
            let src = latch_src.get(node).copied().unwrap_or(usize::MAX);
            if src < blocks {
                if let Some(s) = stamp.get_mut(src) {
                    if *s != cur_stamp {
                        *s = cur_stamp;
                        loop_members.push(src);
                        work.push(src);
                    }
                }
            }
            node = latch_next.get(node).copied().unwrap_or(usize::MAX);
        }

        let mut natural = true;
        while let Some(u) = work.pop() {
            let start = pred_starts.get(u).copied().unwrap_or(0);
            let end = pred_starts.get(u + 1).copied().unwrap_or(start);
            if let Some(preds) = pred_list.get(start..end) {
                for &p in preds {
                    if p < hdr {
                        natural = false;
                        break;
                    }
                    if let Some(s) = stamp.get_mut(p) {
                        if *s != cur_stamp {
                            *s = cur_stamp;
                            loop_members.push(p);
                            work.push(p);
                        }
                    }
                }
            }
            if !natural {
                break;
            }
        }

        if natural {
            for &m in loop_members.iter() {
                if !is_ret.get(m).copied().unwrap_or(false) {
                    if let Some(d) = depth.get_mut(m) {
                        *d = d.saturating_add(1);
                    }
                }
            }
        }
    }

    depth
}

/// Coarse O(B) block-ID interval loop-depth estimate for mega-functions
/// (`blocks > EXACT_LOOP_DEPTH_MAX_BLOCKS`).
fn compute_coarse_loop_depths<'a>(
    view: &View<'_>,
    is_ret: &[bool],
    alloc: Alloc<'a>,
) -> Vec<i32, Alloc<'a>> {
    let blocks = view.blocks();
    let mut depth_diff: Vec<i32, Alloc<'a>> = Vec::with_capacity_in(blocks + 1, alloc);
    for _ in 0..=blocks {
        depth_diff.push(0);
    }

    // Estimate loop nesting depth from backward CFG edges (`target <= b`, excluding `Op::Return`
    // exits which have no outgoing edges and can never be loop headers) in O(B) via a difference
    // array.
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        match view.op(term) {
            Op::Jump => {
                if let Some(&at_raw) = view.data.get(term) {
                    if let Some(&t_raw) = view.extra.get(idx(at_raw)) {
                        let t = idx(t_raw);
                        if t <= b && !is_ret.get(t).copied().unwrap_or(false) {
                            if let Some(s) = depth_diff.get_mut(t) {
                                *s = s.saturating_add(1);
                            }
                            if let Some(e) = depth_diff.get_mut(b + 1) {
                                *e = e.saturating_sub(1);
                            }
                        }
                    }
                }
            }
            Op::Br => {
                if let Some(&at_raw) = view.data.get(term) {
                    let at = idx(at_raw);
                    if let (Some(&t_raw), Some(&t_argc_raw)) =
                        (view.extra.get(at + 1), view.extra.get(at + 2))
                    {
                        let t = idx(t_raw);
                        let t_argc = idx(t_argc_raw);
                        if let Some(&e_raw) = view.extra.get(at + 3 + t_argc) {
                            let e = idx(e_raw);
                            for succ in [t, e] {
                                if succ <= b && !is_ret.get(succ).copied().unwrap_or(false) {
                                    if let Some(s) = depth_diff.get_mut(succ) {
                                        *s = s.saturating_add(1);
                                    }
                                    if let Some(end_slot) = depth_diff.get_mut(b + 1) {
                                        *end_slot = end_slot.saturating_sub(1);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut depth: Vec<i32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut cur_depth: i32 = 0;
    for b in 0..blocks {
        cur_depth = cur_depth.saturating_add(depth_diff.get(b).copied().unwrap_or(0));
        let b_depth = if is_ret.get(b).copied().unwrap_or(false) {
            0
        } else {
            cur_depth.max(0)
        };
        depth.push(b_depth);
    }
    depth
}

/// Computes an ExtTSP-inspired hot-trace basic-block emission order over `0..blocks`:
/// - Always keeps entry block `0` first.
/// - Weights edges by loop nesting depth and branch hints (`EXPECT_TRUE`/`EXPECT_FALSE`, loop
///   back-edges, non-cold successors), greedily extending hot chains so hot fallthrough edges sit
///   adjacent in I-cache with zero taken-branch overhead.
/// - Sinks all cold/trap/unreachable blocks to the tail in deterministic order.
pub(crate) fn compute_block_order<'a>(view: &View<'_>, alloc: Alloc<'a>) -> Vec<usize, Alloc<'a>> {
    let blocks = view.blocks();
    if blocks <= 2 {
        let mut order: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
        for b in 0..blocks {
            order.push(b);
        }
        return order;
    }

    let mut is_cold: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut is_ret: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        is_cold.push(is_cold_block(view, b));
        let term = view.terminator(Block(u32_of(b)));
        is_ret.push(matches!(view.op(term), Op::Return | Op::TailCall | Op::TailCallIndirect));
    }

    let depth = if blocks <= EXACT_LOOP_DEPTH_MAX_BLOCKS {
        compute_exact_loop_depths(view, &is_ret, alloc)
    } else {
        compute_coarse_loop_depths(view, &is_ret, alloc)
    };

    // Bucket hot blocks by clamped loop depth `0..=4` (in ascending block id order) so step 2
    // selects the highest-weight unplaced hot seed in O(B) total instead of O(B^2).
    let mut buckets: [Vec<usize, Alloc<'a>>; 5] = [
        Vec::new_in(alloc),
        Vec::new_in(alloc),
        Vec::new_in(alloc),
        Vec::new_in(alloc),
        Vec::new_in(alloc),
    ];
    for (b, &cold) in is_cold.iter().enumerate() {
        if !cold {
            let b_depth = depth.get(b).copied().unwrap_or(0);
            let d = usize::try_from(b_depth.clamp(0, 4)).unwrap_or(0);
            if let Some(bucket) = buckets.get_mut(d) {
                bucket.push(b);
            }
        }
    }

    // Pick the single hottest outgoing successor for each hot block (ExtTSP greedy trace step).
    let best_hot_succ = |b: usize, placed: &[bool]| -> Option<usize> {
        let term = view.terminator(Block(u32_of(b)));
        let fl = blaze_lir::Flags(*view.flags.get(term)?);
        let is_hot_unplaced = |t: usize| -> bool {
            t < blocks
                && !placed.get(t).copied().unwrap_or(true)
                && !is_cold.get(t).copied().unwrap_or(true)
        };
        match view.op(term) {
            Op::Jump => {
                let at = idx(*view.data.get(term)?);
                let t = idx(*view.extra.get(at)?);
                is_hot_unplaced(t).then_some(t)
            }
            Op::Invoke | Op::InvokeIndirect => {
                let at = idx(*view.data.get(term)?);
                let t = idx(*view.extra.get(at + 2)?);
                is_hot_unplaced(t).then_some(t)
            }
            Op::Br => {
                let at = idx(*view.data.get(term)?);
                let t = idx(*view.extra.get(at + 1)?);
                let t_argc = idx(*view.extra.get(at + 2)?);
                let e = idx(*view.extra.get(at + 3 + t_argc)?);
                let t_cold = is_cold.get(t).copied().unwrap_or(false);
                let e_cold = is_cold.get(e).copied().unwrap_or(false);
                let t_ret = is_ret.get(t).copied().unwrap_or(false);
                let e_ret = is_ret.get(e).copied().unwrap_or(false);
                let dt = depth.get(t).copied().unwrap_or(0);
                let de = depth.get(e).copied().unwrap_or(0);
                let (tw, ew) = if fl.contains(blaze_lir::Flags::EXPECT_TRUE) || (e < blocks && e_cold) {
                    (95u64, 5u64)
                } else if fl.contains(blaze_lir::Flags::EXPECT_FALSE) || (t < blocks && t_cold) {
                    (5u64, 95u64)
                } else if t <= b && e > b && !t_ret {
                    (90, 10)
                } else if e <= b && t > b && !e_ret {
                    (10, 90)
                } else if dt > de {
                    (85, 15)
                } else if de > dt {
                    (15, 85)
                } else if t_ret && !e_ret {
                    (20, 80)
                } else if e_ret && !t_ret {
                    (80, 20)
                } else {
                    (55, 45)
                };
                let t_ok = is_hot_unplaced(t);
                let e_ok = is_hot_unplaced(e);
                match (t_ok, e_ok) {
                    (true, true) => Some(if tw >= ew { t } else { e }),
                    (true, false) => Some(t),
                    (false, true) => Some(e),
                    (false, false) => None,
                }
            }
            _ => None,
        }
    };

    let mut placed: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        placed.push(false);
    }
    let mut order: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);

    // 1. Grow hot trace starting from entry block `0`.
    let mut cur = Some(0usize);
    while let Some(b) = cur {
        if let Some(slot) = placed.get_mut(b) {
            *slot = true;
        }
        order.push(b);
        cur = best_hot_succ(b, &placed);
    }

    // 2. Repeatedly pick the highest-weight unplaced hot block (ties broken by ascending block id)
    //    and extend its greedy hot fallthrough chain — O(B) total via monotonic per-depth cursors.
    let mut cursors = [0usize; 5];
    for d in (0..5usize).rev() {
        let (Some(bucket), Some(cursor)) = (buckets.get(d), cursors.get_mut(d)) else { continue };
        while let Some(&b) = bucket.get(*cursor) {
            *cursor += 1;
            if placed.get(b).copied().unwrap_or(true) {
                continue;
            }
            let mut cur_b = b;
            loop {
                if let Some(slot) = placed.get_mut(cur_b) {
                    *slot = true;
                }
                order.push(cur_b);
                match best_hot_succ(cur_b, &placed) {
                    Some(next) => cur_b = next,
                    None => break,
                }
            }
        }
    }

    // 2b. Zero-duplication latch-before-header fall-through placement (bounded to `blocks <= 2048`
    //     to keep mega-functions strictly O(B) per PR #2400):
    //     When a tiny call-free single-predecessor back-edge latch `l` (`Jump(h)`, `h < l`,
    //     `h != 0`) is entered only from a conditional `Br` inside the loop, placing `l`
    //     immediately before `h` eliminates the unconditional `b h` on EVERY loop iteration
    //     with ZERO code duplication and ZERO branch-predictor splitting, while letting the
    //     loop-exit branch fall straight through into the post-loop block!
    if blocks <= 2048 {
    let mut pred_count: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut only_pred: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut has_latch_before: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        pred_count.push(0);
        only_pred.push(usize::MAX);
        has_latch_before.push(false);
    }
    let record_pred = |pred_count: &mut Vec<u32, Alloc<'a>>,
                       only_pred: &mut Vec<usize, Alloc<'a>>,
                       from: usize,
                       to: usize| {
        if let Some(cnt) = pred_count.get_mut(to) {
            *cnt = cnt.saturating_add(1);
            if *cnt == 1 {
                if let Some(op) = only_pred.get_mut(to) {
                    *op = from;
                }
            }
        }
    };
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        match view.op(term) {
            Op::Jump => {
                if let Some(&at_raw) = view.data.get(term) {
                    if let Some(&t_raw) = view.extra.get(idx(at_raw)) {
                        record_pred(&mut pred_count, &mut only_pred, b, idx(t_raw));
                    }
                }
            }
            Op::Br => {
                if let Some(&at_raw) = view.data.get(term) {
                    let at = idx(at_raw);
                    if let (Some(&t_raw), Some(&t_argc_raw)) =
                        (view.extra.get(at + 1), view.extra.get(at + 2))
                    {
                        let t = idx(t_raw);
                        let t_argc = idx(t_argc_raw);
                        record_pred(&mut pred_count, &mut only_pred, b, t);
                        if let Some(&e_raw) = view.extra.get(at + 3 + t_argc) {
                            record_pred(&mut pred_count, &mut only_pred, b, idx(e_raw));
                        }
                    }
                }
            }
            Op::Invoke | Op::InvokeIndirect => {
                if let Some(&at_raw) = view.data.get(term) {
                    let at = idx(at_raw);
                    if let Some(&norm_raw) = view.extra.get(at + 2) {
                        record_pred(&mut pred_count, &mut only_pred, b, idx(norm_raw));
                    }
                    if let Some(&unw_raw) = view.extra.get(at + 3) {
                        record_pred(&mut pred_count, &mut only_pred, b, idx(unw_raw));
                    }
                }
            }
            Op::Switch | Op::AsmBr => {
                // Conservative: mark every block as multi-pred so switch/asmbr targets are never moved.
                for cnt in pred_count.iter_mut() {
                    *cnt = cnt.saturating_add(2);
                }
            }
            _ => {}
        }
    }

    for l in 1..blocks {
        if is_cold.get(l).copied().unwrap_or(true) {
            continue;
        }
        if pred_count.get(l).copied().unwrap_or(0) != 1 {
            continue;
        }
        let p = only_pred.get(l).copied().unwrap_or(usize::MAX);
        if p >= blocks || p == l {
            continue;
        }
        let p_term = view.terminator(Block(u32_of(p)));
        if view.op(p_term) != Op::Br {
            continue;
        }
        // Extract `p`'s OTHER branch target so we only hoist `l` before `h` when `p`'s other arm
        // lies forward of `p` (avoiding breaking `p -> l` fallthrough into a `cbz; b` pair on
        // multi-exit loops like hash probing).
        let Some(&p_at_raw) = view.data.get(p_term) else { continue };
        let p_at = idx(p_at_raw);
        let (Some(&pt_raw), Some(&pt_argc_raw)) = (view.extra.get(p_at + 1), view.extra.get(p_at + 2)) else {
            continue;
        };
        let pt = idx(pt_raw);
        let Some(&pe_raw) = view.extra.get(p_at + 3 + idx(pt_argc_raw)) else { continue };
        let pe = idx(pe_raw);
        let other_arm = if pt == l { pe } else if pe == l { pt } else { continue };
        let l_range = view.block(Block(u32_of(l)));
        if l_range.len() > 8 {
            continue;
        }
        let l_term = view.terminator(Block(u32_of(l)));
        if view.op(l_term) != Op::Jump {
            continue;
        }
        let mut l_has_call = false;
        for inst in l_range {
            if matches!(
                view.op(inst),
                Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect | Op::TailCall | Op::TailCallIndirect
            ) {
                l_has_call = true;
                break;
            }
        }
        if l_has_call {
            continue;
        }
        let Some(&at_raw) = view.data.get(l_term) else { continue };
        let Some(&h_raw) = view.extra.get(idx(at_raw)) else { continue };
        let h = idx(h_raw);
        if h == 0 || h >= l || is_cold.get(h).copied().unwrap_or(true) {
            continue;
        }
        if has_latch_before.get(h).copied().unwrap_or(true) {
            continue;
        }
        // Only hoist `l` immediately before `h` when `p`'s other arm lies after `p` in `order`
        // (so `p` does not regress from fallthrough into a back-to-back `cbz; b` branch pair).
        let mut pos_h = None;
        let mut pos_l = None;
        let mut pos_p = None;
        let mut pos_other = None;
        for (pos, &blk) in order.iter().enumerate() {
            if blk == h {
                pos_h = Some(pos);
            } else if blk == l {
                pos_l = Some(pos);
            }
            if blk == p {
                pos_p = Some(pos);
            }
            if blk == other_arm {
                pos_other = Some(pos);
            }
        }
        if let (Some(ph), Some(pl), Some(pp), Some(po)) = (pos_h, pos_l, pos_p, pos_other) {
            if ph > 0 && ph < pl && po > pp {
                let moved = order.remove_shift(pl);
                order.insert(ph, moved);
                if let Some(slot) = has_latch_before.get_mut(h) {
                    *slot = true;
                }
            }
        }
    }
    }

    // 3. Append all cold/trap/unreachable blocks at the tail in deterministic order.
    for (b, &is_p) in placed.iter().enumerate() {
        if !is_p {
            order.push(b);
        }
    }
    order
}

/// Marks direct/indirect call instructions eligible for Tail-Call Optimization (`b` / `br` after
/// epilogue instead of `bl`/`blr` + epilogue + `ret`).
///
/// A call at `inst` is tail-callable when:
/// - The immediately following instruction in the same block is `Op::Return` returning the call's
///   exact result (or void for void calls).
/// - Callee signature has the same C ABI, no `sret`/`byval`, and matching return types/extensions.
/// - No stack slots are used in the function (guarantees no pointer to a local stack slot can
///   escape into the tail-called callee after the frame is torn down!).
/// - For direct `Op::Call`, `allow_direct_tco` is `true` (direct `Branch26` mode for export).
pub(crate) fn detect_tail_calls<'a>(
    view: &View<'_>,
    allow_direct_tco: bool,
    os: crate::Os,
    alloc: Alloc<'a>,
) -> Vec<bool, Alloc<'a>> {
    let len = view.len();
    let mut is_tail: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for inst in 0..len {
        is_tail.push(matches!(view.op(inst), Op::TailCall | Op::TailCallIndirect));
    }
    // Never implicitly tail-call if the function allocates local stack slots (a slot address might be passed).
    // Explicit `Op::TailCall`/`Op::TailCallIndirect` (RFC 3407 `become`) already dropped all locals and
    // stays marked true above.
    if !view.slots.is_empty() {
        return is_tail;
    }
    let func_sig_id = SigId(view.head.sig);
    let func_sig = view.sig(func_sig_id);
    if Conv::from_byte(func_sig.conv) != Some(Conv::C) {
        return is_tail;
    }
    let has_stacked = |sig: blaze_lir::func::Sig<'_>| -> bool {
        let mut regs = crate::pass::Regs::new(os);
        for p in sig.params {
            if ParamKind::from_byte(p.kind) == Some(ParamKind::Sret) {
                continue;
            }
            if regs.next(Ty(p.ty)).is_none() {
                return true;
            }
        }
        false
    };
    if func_sig.params.iter().any(|p| ParamKind::from_byte(p.kind) != Some(ParamKind::Value)) || has_stacked(func_sig) {
        return is_tail;
    }

    for b in 0..view.blocks() {
        let range = view.block(Block(u32_of(b)));
        if range.len() < 2 {
            continue;
        }
        let term = range.end - 1;
        let prev = term - 1;
        if view.op(term) != Op::Return {
            continue;
        }
        let op = view.op(prev);
        let eligible_op = (op == Op::Call && allow_direct_tco) || op == Op::CallIndirect;
        if !eligible_op {
            continue;
        }
        let Some(&call_at_raw) = view.data.get(prev) else {
            continue;
        };
        let call_at = idx(call_at_raw);
        let Some(&call_sig_raw) = view.extra.get(call_at + 1) else {
            continue;
        };
        let call_sig_id = SigId(call_sig_raw);
        let call_sig = view.sig(call_sig_id);
        if Conv::from_byte(call_sig.conv) != Some(Conv::C) {
            continue;
        }
        if call_sig.params.iter().any(|p| ParamKind::from_byte(p.kind) != Some(ParamKind::Value)) || has_stacked(call_sig) {
            continue;
        }
        // Return signatures must match identically (types, count, sign/zero ext).
        if call_sig.rets.len() != func_sig.rets.len() {
            continue;
        }
        let rets_match = call_sig.rets.iter().zip(func_sig.rets.iter()).all(|(a, b)| {
            a.ty == b.ty && a.kind == b.kind && a.ext == b.ext
        });
        if !rets_match {
            continue;
        }
        let Some(&ret_at_raw) = view.data.get(term) else {
            continue;
        };
        let ret_at = idx(ret_at_raw);
        let Some(&retc_raw) = view.extra.get(ret_at) else {
            continue;
        };
        let retc = idx(retc_raw);
        let returns_call = match retc {
            0 => view.tys.get(prev).copied().map(Ty) == Some(Ty::NONE),
            1 => view.extra.get(ret_at + 1).copied().map(idx) == Some(prev),
            _ => false,
        };
        if returns_call {
            if let Some(slot) = is_tail.get_mut(prev) {
                *slot = true;
            }
        }
    }
    is_tail
}

/// Checks if a switch terminator qualifies for a dense PC-relative jump table (`O(1)` dispatch).
///
/// Returns `Some((min_val, span))` where `span = (max_val - min_val) as usize + 1` when:
/// - `num_cases >= 4`
/// - `max_val - min_val <= 255` and `span <= num_cases * 2 + 4` (dense enough to be compact).
pub(crate) fn dense_switch_plan(view: &View<'_>, inst: usize) -> Option<(u64, usize)> {
    let at = idx(*view.data.get(inst)?);
    let scrutinee = *view.extra.get(at)?;
    let default_argc = idx(*view.extra.get(at + 2)?);
    let cases_at = at + 3 + default_argc;
    let num_cases = idx(*view.extra.get(cases_at)?);
    if num_cases < 4 {
        return None;
    }
    let width_words = idx(Ty(*view.tys.get(idx(scrutinee))?).const_words());
    if width_words == 0 || width_words > 2 {
        return None;
    }
    let mut pos = cases_at + 1;
    let mut min_v = u64::MAX;
    let mut max_v = 0u64;
    for _ in 0..num_cases {
        let mut case_val = 0u64;
        for w in (0..width_words).rev() {
            case_val = (case_val << 32) | u64::from(*view.extra.get(pos + w)?);
        }
        pos += width_words;
        let case_argc = idx(*view.extra.get(pos + 1)?);
        pos += 2 + case_argc;
        if case_val < min_v {
            min_v = case_val;
        }
        if case_val > max_v {
            max_v = case_val;
        }
    }
    let diff = max_v.checked_sub(min_v)?;
    if diff > 255 {
        return None;
    }
    let span = usize::try_from(diff).ok()? + 1;
    if span <= num_cases * 2 + 4 {
        Some((min_v, span))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use blaze_lir::{
        Flags, IntCond, Op, Ty,
        builder::{Builder, Full, Param, Variadic},
        func::{BlockKind, Conv},
    };
    use blaze_own::Alloc;

    use super::{compute_block_order, compute_exact_loop_depths};

    #[test]
    fn compute_block_order_prefers_loop_body_over_return_exit() {
        let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();

        // 1. Nested loop (`hash_probe_batch` shape):
        //    `outer_hdr` branches `(done -> ret_blk, else -> probe_init)`,
        //    `probe_init` jumps to `inner_hdr`,
        //    `inner_hdr` branches `(hit -> hit_blk, else -> miss_step)`,
        //    `miss_step` branches `(empty -> outer_hdr, else -> inner_hdr)`.
        //    Loop-depth-aware layout must place `outer_hdr -> probe_init -> inner_hdr -> miss_step`
        //    straight through, sinking `ret_blk` out of the hot trace!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let probe_init = b.create_block(BlockKind::Normal);
            let inner_hdr = b.create_block(BlockKind::Normal);
            let miss_step = b.create_block(BlockKind::Normal);
            let hit_blk = b.create_block(BlockKind::Normal);
            let ret_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((outer_hdr, &[zero, seed]));

            b.begin(outer_hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Eq, i, n);
            b.br(Flags::NONE, done, (ret_blk, &[acc]), (probe_init, &[]));

            b.begin(probe_init);
            let c1 = b.iconst(Ty::I64, 1);
            let slot0 = b.binary(Op::Xor, Flags::NONE, i, c1);
            b.jump((inner_hdr, &[slot0]));

            b.begin(inner_hdr);
            let slot = b.param(Ty::I64);
            let is_hit = b.icmp(IntCond::Eq, slot, acc);
            b.br(Flags::NONE, is_hit, (hit_blk, &[]), (miss_step, &[]));

            b.begin(miss_step);
            let next_slot = b.binary(Op::Add, Flags::NONE, slot, c1);
            let is_empty = b.icmp(IntCond::Eq, next_slot, zero);
            let next_i = b.binary(Op::Add, Flags::NONE, i, c1);
            b.br(Flags::NONE, is_empty, (outer_hdr, &[next_i, acc]), (inner_hdr, &[next_slot]));

            b.begin(hit_blk);
            let hit_acc = b.binary(Op::Add, Flags::NONE, acc, c1);
            let hit_i = b.binary(Op::Add, Flags::NONE, i, c1);
            b.jump((outer_hdr, &[hit_i, hit_acc]));

            b.begin(ret_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("hash_probe_batch CFG builds");
            let order = compute_block_order(&func.view(), alloc);
            let pos = |blk: blaze_lir::Block| {
                let target = usize::try_from(blk.0).unwrap();
                order.iter().position(|&x| x == target).expect("block must appear in order")
            };
            assert_eq!(pos(probe_init), pos(outer_hdr) + 1, "probe_init must immediately follow outer_hdr, order={order:?}");
            assert_eq!(pos(inner_hdr), pos(probe_init) + 1, "inner_hdr must immediately follow probe_init, order={order:?}");
            assert_eq!(pos(miss_step), pos(inner_hdr) + 1, "miss_step must immediately follow inner_hdr, order={order:?}");
            assert!(pos(ret_blk) > pos(miss_step), "ret_blk must be placed after the hot loop trace, order={order:?}");
        }

        // 2. Return exit allocated BEFORE loop header (`ret_blk.0 < outer_hdr.0`):
        //    `outer_hdr -> ret_blk` must NOT be mistaken for a backward loop edge!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let ret_blk = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let body = b.create_block(BlockKind::Normal);
            let latch = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((outer_hdr, &[zero]));

            b.begin(ret_blk);
            let rv = b.param(Ty::I64);
            b.ret(&[rv]);

            b.begin(outer_hdr);
            let i = b.param(Ty::I64);
            let done = b.icmp(IntCond::Eq, i, n);
            b.br(Flags::NONE, done, (ret_blk, &[i]), (body, &[]));

            b.begin(body);
            let c1 = b.iconst(Ty::I64, 1);
            let next_i = b.binary(Op::Add, Flags::NONE, i, c1);
            b.jump((latch, &[next_i]));

            b.begin(latch);
            let li = b.param(Ty::I64);
            let keep_going = b.icmp(IntCond::Ult, li, n);
            b.br(Flags::NONE, keep_going, (outer_hdr, &[li]), (ret_blk, &[li]));

            let func = b.finish_checked().expect("early-id return CFG builds");
            let order = compute_block_order(&func.view(), alloc);
            let pos = |blk: blaze_lir::Block| {
                let target = usize::try_from(blk.0).unwrap();
                order.iter().position(|&x| x == target).expect("block must appear in order")
            };
            assert_eq!(pos(body), pos(outer_hdr) + 1, "body must immediately follow outer_hdr even when ret_blk.0 < outer_hdr.0, order={order:?}");
            assert_eq!(pos(latch), pos(body) + 1, "latch must immediately follow body, order={order:?}");
            assert!(pos(ret_blk) > pos(latch), "ret_blk must follow loop body, order={order:?}");
        }

        // 3. Equal-depth branch (`dt == de == 0`) between `Op::Return` exit and non-return body:
        //    non-return continuation must immediately follow entry.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let ret_blk = b.create_block(BlockKind::Normal);
            let work_blk = b.create_block(BlockKind::Normal);
            let done_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let x = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let is_zero = b.icmp(IntCond::Eq, x, zero);
            b.br(Flags::NONE, is_zero, (ret_blk, &[zero]), (work_blk, &[]));

            b.begin(ret_blk);
            let rv = b.param(Ty::I64);
            b.ret(&[rv]);

            b.begin(work_blk);
            let c1 = b.iconst(Ty::I64, 1);
            let y = b.binary(Op::Add, Flags::NONE, x, c1);
            b.jump((done_blk, &[y]));

            b.begin(done_blk);
            let dv = b.param(Ty::I64);
            b.ret(&[dv]);

            let func = b.finish_checked().expect("equal-depth return CFG builds");
            let order = compute_block_order(&func.view(), alloc);
            let pos = |blk: blaze_lir::Block| {
                let target = usize::try_from(blk.0).unwrap();
                order.iter().position(|&x| x == target).expect("block must appear in order")
            };
            assert_eq!(pos(work_blk), pos(entry) + 1, "non-return work_blk must immediately follow entry, order={order:?}");
            assert!(pos(ret_blk) > pos(work_blk), "early ret_blk must follow work_blk, order={order:?}");
        }
    }

    #[test]
    fn compute_block_order_exact_natural_loop_depth_sinks_inner_exit_hit_block() {
        let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();

        // Exact `hash_probe_batch` CFG where `hit_blk` (b4) has block ID BETWEEN `inner_hdr` (b3)
        // and `miss_step` (b6):
        //   entry (b0) -> outer_hdr (b1)
        //   outer_hdr (b1): br(done -> ret_blk (b7), else -> probe_init (b2))
        //   probe_init (b2) -> inner_hdr (b3)
        //   inner_hdr (b3): br(is_hit -> hit_blk (b4), else -> miss_blk (b5))
        //   hit_blk (b4) -> outer_hdr (b1)          [exits inner loop! true depth = 1, >8 insts]
        //   miss_blk (b5): br(is_empty -> outer_hdr (b1), else -> miss_step (b6))
        //   miss_step (b6) -> inner_hdr (b3)        [inner latch! true depth = 2]
        //   ret_blk (b7): ret
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let outer_hdr = b.create_block(BlockKind::Normal);
        let probe_init = b.create_block(BlockKind::Normal);
        let inner_hdr = b.create_block(BlockKind::Normal);
        let hit_blk = b.create_block(BlockKind::Normal);
        let miss_blk = b.create_block(BlockKind::Normal);
        let miss_step = b.create_block(BlockKind::Normal);
        let ret_blk = b.create_block(BlockKind::Normal);
        assert!(
            hit_blk.0 > inner_hdr.0 && hit_blk.0 < miss_step.0,
            "hit_blk must lie between inner_hdr and miss_step by block ID"
        );

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        b.jump((outer_hdr, &[zero, seed]));

        b.begin(outer_hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Eq, i, n);
        b.br(Flags::NONE, done, (ret_blk, &[acc]), (probe_init, &[]));

        b.begin(probe_init);
        let slot0 = b.binary(Op::Xor, Flags::NONE, i, c1);
        b.jump((inner_hdr, &[slot0]));

        b.begin(inner_hdr);
        let slot = b.param(Ty::I64);
        let is_hit = b.icmp(IntCond::Eq, slot, acc);
        b.br(Flags::NONE, is_hit, (hit_blk, &[]), (miss_blk, &[]));

        // Give `hit_blk` > 8 instructions so Step 2b latch-hoisting does not mask loop-depth layout.
        b.begin(hit_blk);
        let h0 = b.binary(Op::Add, Flags::NONE, acc, slot);
        let h1 = b.binary(Op::Xor, Flags::NONE, h0, i);
        let h2 = b.binary(Op::Add, Flags::NONE, h1, c1);
        let h3 = b.binary(Op::Xor, Flags::NONE, h2, slot);
        let h4 = b.binary(Op::Add, Flags::NONE, h3, c1);
        let h5 = b.binary(Op::Xor, Flags::NONE, h4, i);
        let hit_acc = b.binary(Op::Add, Flags::NONE, h5, c1);
        let hit_i = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((outer_hdr, &[hit_i, hit_acc]));

        b.begin(miss_blk);
        let is_empty = b.icmp(IntCond::Eq, slot, zero);
        let miss_i = b.binary(Op::Add, Flags::NONE, i, c1);
        b.br(Flags::NONE, is_empty, (outer_hdr, &[miss_i, acc]), (miss_step, &[]));

        b.begin(miss_step);
        let next_slot = b.binary(Op::Add, Flags::NONE, slot, c1);
        b.jump((inner_hdr, &[next_slot]));

        b.begin(ret_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("hash_probe_batch inner-exit CFG builds");
        let view = func.view();

        let is_ret = [false, false, false, false, false, false, false, true];
        let depths = compute_exact_loop_depths(&view, &is_ret, alloc);
        assert_eq!(depths.as_slice(), &[0, 1, 1, 2, 1, 2, 2, 0]);

        let order = compute_block_order(&view, alloc);
        let pos = |blk: blaze_lir::Block| {
            let target = usize::try_from(blk.0).unwrap();
            order.iter().position(|&x| x == target).expect("block must appear in order")
        };
        assert_eq!(pos(entry), 0, "entry must be first, order={order:?}");
        assert_eq!(pos(outer_hdr), pos(entry) + 1, "outer_hdr must follow entry, order={order:?}");
        assert_eq!(pos(probe_init), pos(outer_hdr) + 1, "probe_init must follow outer_hdr, order={order:?}");
        assert_eq!(pos(inner_hdr), pos(probe_init) + 1, "inner_hdr must follow probe_init, order={order:?}");
        assert_eq!(pos(miss_blk), pos(inner_hdr) + 1, "miss_blk must follow inner_hdr, order={order:?}");
        assert_eq!(pos(miss_step), pos(miss_blk) + 1, "miss_step must follow miss_blk, order={order:?}");
        assert!(pos(hit_blk) > pos(miss_step), "hit_blk must be sunk after miss_step, order={order:?}");
        assert!(pos(ret_blk) > pos(hit_blk), "ret_blk must be placed after hit_blk, order={order:?}");
    }
}

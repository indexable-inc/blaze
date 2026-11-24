//! Async Coroutine Fusion, Cross-Suspend Frame Graph Coloring & Sync-Completion Fast Path
//! (`blaze_lir::coro_opt`).
//!
//! Two complementary mid-end optimizations over unsplit coroutine LIR (`Op::Suspend` /
//! `Op::Cancelled`):
//!
//! 1. **Cross-Suspend Frame Graph Coloring ([`pack_coroutine_frame_colored`])**:
//!    - Computes exact per-`Op::Suspend` liveness across the CFG + dominator tree: an SSA value
//!      (non-void, non-entry-parameter, non-rematerializable constant) is live across suspend `s`
//!      iff its definition dominates `s`'s block and some use of it is reachable from `s`'s resume
//!      or cancel target without re-entering its defining block.
//!    - Constructs the exact interference graph between cross-suspend values (edge `(u, v)` iff
//!      some suspend `s` has both `u` and `v` simultaneously live across `s`).
//!    - Greedily packs non-conflicting values into overlapping byte ranges ordered by descending
//!      `(align, size)` then ascending instruction index (deterministic, Rule 25), reporting both
//!      `naive_bytes` and `colored_bytes`.
//!    - Computes `can_sync_complete`: whether an entry-to-`Op::Return` path exists that visits
//!      zero `Op::Suspend` instructions (enabling a zero-frame-write synchronous fast path).
//!
//! 2. **Nested Coroutine Fusion ([`fuse_nested_coroutines`])**:
//!    - Finds direct `Op::Call` sites in `outer` targeting `child_ref_id` and splices `child`'s
//!      entire CFG — including its `Op::Suspend` and `Op::Cancelled` blocks — directly into
//!      `outer`, binding `child`'s entry parameters to the call arguments, renumbering fused
//!      suspend points uniquely, redirecting `child`'s `Op::Return` edges into the post-call
//!      continuation block, and verifying the resulting flat coroutine via [`verify`].
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;
use core::cmp::Reverse;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, BlockKind, Func, Op, Ty, View,
    func::{
        AsmOperandKind, AsmOperandRec, AsmRec, CompoundRec, DataRec, DataRelocRec, IntrinsicRec,
        MemAttrsRec, ParamFactsRec, ParamRec, RefKind, RefRec, SigId, SigRec,
    },
    op::{Field, Layout},
    ty::Form,
    verify,
};

/// Telemetry from [`pack_coroutine_frame_colored`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct CoroFrameStats {
    pub saved_values: u32,
    pub naive_bytes: u32,
    pub colored_bytes: u32,
    pub can_sync_complete: bool,
}

/// Frame slot assignment for one cross-suspend SSA value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoroFrameSlot {
    pub inst: u32,
    pub offset: u32,
    pub size: u32,
    pub align: u32,
}

/// Graph-colored coroutine frame layout produced by [`pack_coroutine_frame_colored`].
#[derive(Debug)]
pub struct CoroFramePlan<A: Allocator> {
    pub slots: Vec<CoroFrameSlot, A>,
    pub total_bytes: u32,
    pub total_align: u32,
    pub stats: CoroFrameStats,
}

/// Telemetry from [`fuse_nested_coroutines`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct CoroFuseStats {
    pub child_suspends_fused: u32,
    pub poll_calls_eliminated: u32,
}

/// Output of [`fuse_nested_coroutines`].
#[derive(Debug)]
pub struct CoroFuseOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: CoroFuseStats,
}

#[inline]
fn to_idx(x: u32) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(x: usize) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

#[inline]
fn try_u32(x: usize, inst: usize, what: &'static str) -> Result<u32, SimplifyError> {
    u32::try_from(x).map_err(|_| SimplifyError::Refused { inst, what })
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL
        .iter()
        .position(|&o| o == op)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0)
}

#[inline]
const fn block_kind_byte(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::Normal => 0,
        BlockKind::Cold => 1,
        BlockKind::Cleanup => 2,
        BlockKind::Catch => 3,
        BlockKind::Terminate => 4,
    }
}

#[inline]
fn at_u32(slice: &[u32], idx: usize) -> u32 {
    slice.get(idx).copied().unwrap_or(0)
}

#[inline]
fn at_u32_or(slice: &[u32], idx: usize, default: u32) -> u32 {
    slice.get(idx).copied().unwrap_or(default)
}

#[inline]
fn at_u16(slice: &[u16], idx: usize) -> u16 {
    slice.get(idx).copied().unwrap_or(0)
}

#[inline]
fn at_u8(slice: &[u8], idx: usize) -> u8 {
    slice.get(idx).copied().unwrap_or(0)
}

#[inline]
fn at_bool(slice: &[bool], idx: usize) -> bool {
    slice.get(idx).copied().unwrap_or(false)
}

#[inline]
fn at_pair(slice: &[(u32, u32)], idx: usize) -> (u32, u32) {
    slice.get(idx).copied().unwrap_or((0, 0))
}

#[inline]
fn set_at<T>(slice: &mut [T], idx: usize, val: T) {
    if let Some(slot) = slice.get_mut(idx) {
        *slot = val;
    }
}

#[inline]
fn req_slice<'a, T>(
    slice: &'a [T],
    start: usize,
    len: usize,
    inst: usize,
    what: &'static str,
) -> Result<&'a [T], SimplifyError> {
    start
        .checked_add(len)
        .and_then(|end| slice.get(start..end))
        .ok_or(SimplifyError::Refused { inst, what })
}

#[inline]
fn align_up(offset: u64, align: u32) -> u64 {
    let a = u64::from(align.max(1));
    (offset + a - 1) & !(a - 1)
}

#[inline]
fn is_rematerializable(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Vconst
            | Op::Null
            | Op::Uninit
            | Op::FuncAddr
            | Op::DataAddr
    )
}

/// Size and byte alignment for spilling a cross-suspend value of `ty`.
#[inline]
fn spill_size_align(ty: Ty, ptr_bits: u32) -> Option<(u32, u32)> {
    if ty == Ty::I1 {
        return Some((1, 1));
    }
    match ty.form() {
        Form::Scalar | Form::Vector => {
            let bits = ty.bits(ptr_bits);
            if bits == 0 || bits % 8 != 0 {
                return None;
            }
            let size = bits / 8;
            let align = size.next_power_of_two().min(16);
            Some((size, align))
        }
        Form::Scalable | Form::Compound => None,
    }
}

/// Block where SSA value `v` is defined (`Invoke`/`InvokeIndirect`/`AsmBr` define at their normal
/// target block's head).
#[inline]
fn def_block_of(view: &View<'_>, block_of: &[u32], v: usize) -> u32 {
    let op = view.op(v);
    if matches!(op, Op::Invoke | Op::InvokeIndirect | Op::AsmBr) {
        let at = to_idx(at_u32(view.data, v));
        if op == Op::AsmBr {
            at_u32(view.extra, at + 1)
        } else {
            at_u32(view.extra, at + 2)
        }
    } else {
        at_u32(block_of, v)
    }
}

/// Dominator tree + CFG adjacency for exact cross-suspend liveness.
struct CfgDom<A: Allocator> {
    succs: Vec<Vec<u32, A>, A>,
    idom: Vec<u32, A>,
}

impl<A: Allocator + Copy> CfgDom<A> {
    fn build(view: &View<'_>, alloc: &A) -> Self {
        let blocks = view.blocks();
        let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
        let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
        for _ in 0..blocks {
            preds.push(Vec::new_in(*alloc));
            succs.push(Vec::new_in(*alloc));
        }
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            let term = view.terminator(Block(b_u32));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let tb = to_idx(edge.block);
                    if let Some(sb) = succs.get_mut(b)
                        && !sb.iter().any(|&s| s == edge.block)
                    {
                        sb.push(edge.block);
                    }
                    if let Some(pb) = preds.get_mut(tb)
                        && !pb.iter().any(|&p| p == b_u32)
                    {
                        pb.push(b_u32);
                    }
                }
            });
        }

        let mut seen = filled(blocks, false, alloc);
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
        let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, *alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, 0));
        }
        while let Some(top) = stack.last_mut() {
            let b_u32 = top.0;
            let b = to_idx(b_u32);
            let idx = top.1;
            let next_s = succs.get(b).and_then(|sb| sb.get(idx).copied());
            if let Some(s) = next_s {
                top.1 += 1;
                let si = to_idx(s);
                if !at_bool(&seen, si) {
                    set_at(&mut seen, si, true);
                    stack.push((s, 0));
                }
            } else {
                post.push(b_u32);
                stack.pop();
            }
        }

        let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), *alloc);
        for &b in post.iter().rev() {
            rpo.push(b);
        }
        let mut order = filled(blocks, NONE, alloc);
        for (i, &b) in rpo.iter().enumerate() {
            set_at(&mut order, to_idx(b), to_u32(i));
        }

        let mut idom = filled(blocks, NONE, alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        for _ in 0..2 * blocks + 2 {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let bi = to_idx(b);
                let mut new_idom = NONE;
                if let Some(pb) = preds.get(bi) {
                    for &p in pb {
                        if at_u32_or(&idom, to_idx(p), NONE) == NONE {
                            continue;
                        }
                        new_idom = match new_idom {
                            NONE => p,
                            cur => intersect_dom(&idom, &order, p, cur),
                        };
                    }
                }
                if new_idom != at_u32_or(&idom, bi, NONE) {
                    set_at(&mut idom, bi, new_idom);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        Self { succs, idom }
    }

    #[inline]
    fn reachable(&self, b: u32) -> bool {
        at_u32_or(&self.idom, to_idx(b), NONE) != NONE
    }

    #[inline]
    fn dominates(&self, a: u32, mut b: u32) -> bool {
        if !self.reachable(a) || !self.reachable(b) {
            return false;
        }
        for _ in 0..self.idom.len() {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = at_u32_or(&self.idom, to_idx(b), NONE);
            if b == NONE {
                return false;
            }
        }
        false
    }
}

#[inline]
fn intersect_dom(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    for _ in 0..2 * idom.len() + 2 {
        if a == b {
            return a;
        }
        while at_u32_or(order, to_idx(a), NONE) > at_u32_or(order, to_idx(b), NONE) {
            let next = at_u32_or(idom, to_idx(a), a);
            if next == a {
                break;
            }
            a = next;
        }
        while at_u32_or(order, to_idx(b), NONE) > at_u32_or(order, to_idx(a), NONE) {
            let next = at_u32_or(idom, to_idx(b), b);
            if next == b {
                break;
            }
            b = next;
        }
    }
    a
}

/// Computes blocks reachable from `roots`, optionally forbidding entry into `blocked`.
fn reachable_from<A: Allocator + Copy>(
    succs: &[Vec<u32, A>],
    roots: &[u32],
    blocked: u32,
    alloc: &A,
) -> Vec<bool, A> {
    let blocks = succs.len();
    let mut seen = filled(blocks, false, alloc);
    let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    for &r in roots {
        let ri = to_idx(r);
        if r != blocked && ri < blocks && !at_bool(&seen, ri) {
            set_at(&mut seen, ri, true);
            work.push(r);
        }
    }
    while let Some(b) = work.pop() {
        if let Some(sb) = succs.get(to_idx(b)) {
            for &s in sb {
                let si = to_idx(s);
                if s != blocked && si < blocks && !at_bool(&seen, si) {
                    set_at(&mut seen, si, true);
                    work.push(s);
                }
            }
        }
    }
    seen
}

/// Computes exact per-`Op::Suspend` liveness, builds the cross-suspend interference graph, and
/// greedily packs non-conflicting values into overlapping frame byte offsets.
pub fn pack_coroutine_frame_colored<A: Allocator + Copy>(
    view: View<'_>,
    prefix_bytes: u32,
    prefix_align: u32,
    alloc: A,
) -> Result<CoroFramePlan<A>, SimplifyError> {
    if prefix_align != 0 && !prefix_align.is_power_of_two() {
        return Err(SimplifyError::Refused {
            inst: 0,
            what: "coroutine prefix alignment is not a power of two",
        });
    }
    let base_align = prefix_align.max(1);
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 || len == 0 {
        let total = u32::try_from(align_up(u64::from(prefix_bytes), base_align)).map_err(|_| {
            SimplifyError::Refused { inst: 0, what: "coroutine frame size overflows u32" }
        })?;
        return Ok(CoroFramePlan {
            slots: Vec::new_in(alloc),
            total_bytes: total,
            total_align: base_align,
            stats: CoroFrameStats {
                saved_values: 0,
                naive_bytes: total,
                colored_bytes: total,
                can_sync_complete: false,
            },
        });
    }

    let dom = CfgDom::build(&view, &alloc);

    // 1. Check `can_sync_complete`: path from entry block 0 to `Op::Return` visiting zero `Op::Suspend`.
    let mut can_sync_complete = false;
    {
        let mut seen = filled(blocks, false, &alloc);
        let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        set_at(&mut seen, 0, true);
        work.push(0);
        while let Some(b) = work.pop() {
            let term = view.terminator(Block(b));
            match view.op(term) {
                Op::Return => {
                    can_sync_complete = true;
                    break;
                }
                Op::Suspend => {}
                _ => {
                    if let Some(sb) = dom.succs.get(to_idx(b)) {
                        for &s in sb {
                            let si = to_idx(s);
                            if !at_bool(&seen, si) {
                                set_at(&mut seen, si, true);
                                work.push(s);
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Map instructions to blocks and record per-value use blocks (unique per value).
    let mut block_of = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut block_of, inst, b_u32);
        }
    }

    let entry_params = view.params(Block(0));
    let mut use_blocks: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        use_blocks.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        if !dom.reachable(b_u32) {
            continue;
        }
        for inst in view.block(Block(b_u32)) {
            each(&view, inst, |item| match item {
                Item::Value(v) => {
                    if let Some(ub) = use_blocks.get_mut(to_idx(v))
                        && !ub.iter().any(|&x| x == b_u32)
                    {
                        ub.push(b_u32);
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let v = to_idx(at_u32(view.extra, start + k));
                            if let Some(ub) = use_blocks.get_mut(v)
                                && !ub.iter().any(|&x| x == b_u32)
                            {
                                ub.push(b_u32);
                            }
                        }
                    }
                }
            });
        }
    }

    // Candidate saveable values: reachable, non-void, non-entry-param, non-rematerializable, used.
    let mut candidates: Vec<u32, A> = Vec::new_in(alloc);
    for v in 0..len {
        if entry_params.contains(&v) {
            continue;
        }
        if Ty(at_u16(view.tys, v)).is_none() || is_rematerializable(view.op(v)) {
            continue;
        }
        if use_blocks.get(v).is_none_or(|ub| ub.is_empty()) {
            continue;
        }
        let def_blk = def_block_of(&view, &block_of, v);
        if !dom.reachable(def_blk) {
            continue;
        }
        candidates.push(to_u32(v));
    }

    // 3. Collect all reachable `Op::Suspend` points and compute exact liveness across each.
    let mut live_at_suspend: Vec<Vec<u32, A>, A> = Vec::new_in(alloc);
    let mut is_saved = filled(len, false, &alloc);

    for sb in 0..blocks {
        let sb_u32 = to_u32(sb);
        if !dom.reachable(sb_u32) {
            continue;
        }
        let term = view.terminator(Block(sb_u32));
        if view.op(term) != Op::Suspend {
            continue;
        }
        let at = to_idx(at_u32(view.data, term));
        let resume_blk = at_u32(view.extra, at + 1);
        let cancel_blk = at_u32(view.extra, at + 2);
        let roots = [resume_blk, cancel_blk];
        let post_all = reachable_from(&dom.succs, &roots, NONE, &alloc);

        let mut live_here: Vec<u32, A> = Vec::new_in(alloc);
        for &v in &candidates {
            let vi = to_idx(v);
            let def_blk = def_block_of(&view, &block_of, vi);
            if !dom.dominates(def_blk, sb_u32) {
                continue;
            }
            let Some(ub_list) = use_blocks.get(vi) else {
                continue;
            };
            let live_across = if !at_bool(&post_all, to_idx(def_blk)) {
                // Fast path: `def_blk` is not reachable after this suspend, so every block in
                // `post_all` is reached without re-entering `def_blk`.
                ub_list.iter().any(|&ub| at_bool(&post_all, to_idx(ub)))
            } else {
                // Loop path: walk from `{resume_blk, cancel_blk}` without entering `def_blk`.
                let post_without_def = reachable_from(&dom.succs, &roots, def_blk, &alloc);
                ub_list.iter().any(|&ub| at_bool(&post_without_def, to_idx(ub)))
            };
            if live_across {
                live_here.push(v);
                set_at(&mut is_saved, vi, true);
            }
        }
        live_at_suspend.push(live_here);
    }

    // 4. Number saved values deterministically in ascending instruction order.
    let mut saved_insts: Vec<u32, A> = Vec::new_in(alloc);
    let mut saved_ord = filled(len, NONE, &alloc);
    for (v, &keep) in is_saved.iter().enumerate() {
        if keep {
            set_at(&mut saved_ord, v, to_u32(saved_insts.len()));
            saved_insts.push(to_u32(v));
        }
    }

    let n = saved_insts.len();
    if n == 0 {
        let total = u32::try_from(align_up(u64::from(prefix_bytes), base_align)).map_err(|_| {
            SimplifyError::Refused { inst: 0, what: "coroutine frame size overflows u32" }
        })?;
        return Ok(CoroFramePlan {
            slots: Vec::new_in(alloc),
            total_bytes: total,
            total_align: base_align,
            stats: CoroFrameStats {
                saved_values: 0,
                naive_bytes: total,
                colored_bytes: total,
                can_sync_complete,
            },
        });
    }

    let mut sizes: Vec<(u32, u32), A> = Vec::with_capacity_in(n, alloc);
    let mut total_align = base_align;
    for &inst in &saved_insts {
        let inst_idx = to_idx(inst);
        let ty = Ty(at_u16(view.tys, inst_idx));
        let Some((size, align)) = spill_size_align(ty, view.head.ptr_bits) else {
            return Err(SimplifyError::Refused {
                inst: inst_idx,
                what: "compound or scalable value live across coroutine suspend",
            });
        };
        sizes.push((size, align));
        total_align = total_align.max(align);
    }

    // 5. Build exact interference bit matrix over saved values.
    let rw = n.div_ceil(64);
    let mut conflicts = filled(n * rw, 0u64, &alloc);
    for live_set in &live_at_suspend {
        let m = live_set.len();
        for a_idx in 0..m {
            let va = at_u32(live_set, a_idx);
            let oa = to_idx(at_u32(&saved_ord, to_idx(va)));
            for b_idx in (a_idx + 1)..m {
                let vb = at_u32(live_set, b_idx);
                let ob = to_idx(at_u32(&saved_ord, to_idx(vb)));
                if let Some(w) = conflicts.get_mut(oa * rw + ob / 64) {
                    *w |= 1u64 << (ob % 64);
                }
                if let Some(w) = conflicts.get_mut(ob * rw + oa / 64) {
                    *w |= 1u64 << (oa % 64);
                }
            }
        }
    }

    // 6. Deterministic ordering by descending `(align, size)` then ascending `inst` (Rule 25).
    let mut order: Vec<usize, A> = Vec::with_capacity_in(n, alloc);
    for i in 0..n {
        order.push(i);
    }
    order.sort_unstable_by_key(|&i| {
        let (sz, al) = at_pair(&sizes, i);
        (Reverse(al), Reverse(sz), at_u32(&saved_insts, i))
    });

    // Naive sequential layout: distinct non-overlapping offset for every saved value.
    let mut naive_at = u64::from(prefix_bytes);
    for &i in &order {
        let (size, align) = at_pair(&sizes, i);
        naive_at = align_up(naive_at, align) + u64::from(size);
    }
    let naive_bytes = u32::try_from(align_up(naive_at, total_align)).map_err(|_| {
        SimplifyError::Refused { inst: 0, what: "naive coroutine frame size overflows u32" }
    })?;

    // Graph-colored interval packing: share byte ranges among non-conflicting cross-suspend values.
    let mut offsets = filled(n, 0u32, &alloc);
    let mut placed = filled(n, false, &alloc);
    let mut taken: Vec<(u64, u64), A> = Vec::with_capacity_in(n, alloc);
    let mut colored_end = u64::from(prefix_bytes);

    for &i in &order {
        let (size, align) = at_pair(&sizes, i);
        taken.clear();
        for j in 0..n {
            let bit = conflicts.get(i * rw + j / 64).copied().unwrap_or(0) & (1u64 << (j % 64));
            if j != i && at_bool(&placed, j) && bit != 0 {
                let start = u64::from(at_u32(&offsets, j));
                let j_size = at_pair(&sizes, j).0;
                taken.push((start, start + u64::from(j_size)));
            }
        }
        taken.sort_unstable();
        let mut at = align_up(u64::from(prefix_bytes), align);
        for &(first, end) in &taken {
            if end <= at {
                continue;
            }
            if first >= at + u64::from(size) {
                break;
            }
            at = align_up(end, align);
        }
        let end = at + u64::from(size);
        let off = u32::try_from(at).map_err(|_| SimplifyError::Refused {
            inst: to_idx(at_u32(&saved_insts, i)),
            what: "colored coroutine frame offset overflows u32",
        })?;
        set_at(&mut offsets, i, off);
        set_at(&mut placed, i, true);
        colored_end = colored_end.max(end);
    }

    let total_bytes = u32::try_from(align_up(colored_end, total_align)).map_err(|_| {
        SimplifyError::Refused { inst: 0, what: "colored coroutine frame size overflows u32" }
    })?;

    let mut slots: Vec<CoroFrameSlot, A> = Vec::with_capacity_in(n, alloc);
    for i in 0..n {
        let (size, align) = at_pair(&sizes, i);
        slots.push(CoroFrameSlot {
            inst: at_u32(&saved_insts, i),
            offset: at_u32(&offsets, i),
            size,
            align,
        });
    }
    slots.sort_unstable_by_key(|s| (s.offset, s.inst));

    Ok(CoroFramePlan {
        slots,
        total_bytes,
        total_align,
        stats: CoroFrameStats {
            saved_values: to_u32(n),
            naive_bytes,
            colored_bytes: total_bytes,
            can_sync_complete,
        },
    })
}

/// Remaps an optional `0 | (1 + row)` table index by `offset`.
#[inline]
fn remap_opt_row(field: u32, offset: u32) -> u32 {
    if field == 0 { 0 } else { field + offset }
}

/// Remaps a child `Ty` whose compound row shifted by `comp_off`.
#[inline]
fn remap_child_ty(ty: Ty, comp_off: u32, inst: usize) -> Result<Ty, SimplifyError> {
    match ty.compound_row() {
        Some(row) => Ty::compound(row + comp_off).ok_or(SimplifyError::Refused {
            inst,
            what: "fused coroutine compound table overflows encoding",
        }),
        None => Ok(ty),
    }
}

/// Table offsets for `child`'s shared tables after appending them onto `out`.
#[derive(Clone, Copy)]
struct ChildTables {
    comp_off: u32,
    mem_off: u32,
    sig_off: u32,
    ref_off: u32,
    intr_off: u32,
    asm_off: u32,
}

fn append_child_shared_tables<A: Allocator>(
    out: &mut Func<A>,
    child: &View<'_>,
) -> Result<ChildTables, SimplifyError> {
    let comp_off = try_u32(out.compounds.len(), 0, "compounds table overflow")?;
    for rec in child.compounds {
        let first = try_u32(out.compound_tys.len(), 0, "compound_tys overflow")?;
        let slice = req_slice(
            child.compound_tys,
            to_idx(rec.first),
            to_idx(rec.len),
            0,
            "compound_tys slice out of bounds",
        )?;
        out.compound_tys.extend_from_slice(slice);
        out.compounds.push(CompoundRec { first, len: rec.len });
    }

    let range_off = try_u32(out.ranges.len(), 0, "ranges table overflow")?;
    out.ranges.extend_from_slice(child.ranges);

    let fact_off = try_u32(out.param_facts.len(), 0, "param_facts overflow")?;
    for &rec in child.param_facts {
        out.param_facts.push(ParamFactsRec {
            range: remap_opt_row(rec.range, range_off),
            ..rec
        });
    }

    let mem_off = try_u32(out.mem_attrs.len(), 0, "mem_attrs overflow")?;
    for &rec in child.mem_attrs {
        out.mem_attrs.push(MemAttrsRec {
            range: remap_opt_row(rec.range, range_off),
            ..rec
        });
    }

    let sig_off = try_u32(out.sigs.len(), 0, "sigs table overflow")?;
    for &rec in child.sigs {
        let first = try_u32(out.sig_params.len(), 0, "sig_params overflow")?;
        let len = usize::from(rec.params) + usize::from(rec.rets);
        let params = req_slice(
            child.sig_params,
            to_idx(rec.first),
            len,
            0,
            "sig_params slice out of bounds",
        )?;
        for &param in params {
            let ty = remap_child_ty(Ty(param.ty), comp_off, 0)?;
            out.sig_params.push(ParamRec {
                facts: remap_opt_row(param.facts, fact_off),
                ty: ty.0,
                ext: param.ext,
                kind: param.kind,
            });
        }
        out.sigs.push(SigRec { first, ..rec });
    }

    let ref_off = try_u32(out.refs.len(), 0, "refs table overflow")?;
    let data_off = try_u32(out.datas.len(), 0, "datas table overflow")?;
    for &rec in child.datas {
        let bytes_first = try_u32(out.data_bytes.len(), 0, "data_bytes overflow")?;
        let bytes = req_slice(
            child.data_bytes,
            to_idx(rec.bytes_first),
            to_idx(rec.bytes_len),
            0,
            "data_bytes slice out of bounds",
        )?;
        out.data_bytes.extend_from_slice(bytes);
        let relocs_first = try_u32(out.data_relocs.len(), 0, "data_relocs overflow")?;
        let relocs = req_slice(
            child.data_relocs,
            to_idx(rec.relocs_first),
            to_idx(rec.relocs_len),
            0,
            "data_relocs slice out of bounds",
        )?;
        for &reloc in relocs {
            out.data_relocs.push(DataRelocRec {
                target: reloc.target + ref_off,
                ..reloc
            });
        }
        let positions_first = try_u32(out.data_positions.len(), 0, "data_positions overflow")?;
        let positions = req_slice(
            child.data_positions,
            to_idx(rec.positions_first),
            to_idx(rec.positions_len),
            0,
            "data_positions slice out of bounds",
        )?;
        out.data_positions.extend_from_slice(positions);
        out.datas.push(DataRec {
            bytes_first,
            bytes_len: rec.bytes_len,
            relocs_first,
            relocs_len: rec.relocs_len,
            align: rec.align,
            positions_first,
            positions_len: rec.positions_len,
        });
    }
    for &rec in child.refs {
        let place = if RefKind::from_word(rec.kind) == Some(RefKind::Data) {
            rec.place + data_off
        } else {
            rec.place
        };
        out.refs.push(RefRec { place, kind: rec.kind });
    }

    let intr_off = try_u32(out.intrinsics.len(), 0, "intrinsics table overflow")?;
    for &rec in child.intrinsics {
        let name_first = try_u32(out.names.len(), 0, "names table overflow")?;
        let name_bytes = req_slice(
            child.names,
            to_idx(rec.name_first),
            to_idx(rec.name_len),
            0,
            "names slice out of bounds",
        )?;
        out.names.extend_from_slice(name_bytes);
        out.intrinsics.push(IntrinsicRec {
            name_first,
            name_len: rec.name_len,
            sig: rec.sig + sig_off,
        });
    }

    let asm_off = try_u32(out.asms.len(), 0, "asms table overflow")?;
    let asm_text_off = try_u32(out.asm_text.len(), 0, "asm_text overflow")?;
    out.asm_text.extend_from_slice(child.asm_text);
    for &rec in child.asms {
        let pieces_first = try_u32(out.asm_pieces.len(), 0, "asm_pieces overflow")?;
        let pieces = req_slice(
            child.asm_pieces,
            to_idx(rec.pieces_first),
            to_idx(rec.pieces_len),
            0,
            "asm_pieces slice out of bounds",
        )?;
        for &piece in pieces {
            let mut p = piece;
            if p.kind == crate::func::AsmPieceRec::TEXT {
                p.a += asm_text_off;
            }
            out.asm_pieces.push(p);
        }
        let operands_first = try_u32(out.asm_operands.len(), 0, "asm_operands overflow")?;
        let operands = req_slice(
            child.asm_operands,
            to_idx(rec.operands_first),
            to_idx(rec.operands_len),
            0,
            "asm_operands slice out of bounds",
        )?;
        for &opnd in operands {
            let mut o = opnd;
            if o.reg_len > 0 {
                o.reg_first += asm_text_off;
            }
            if AsmOperandKind::from_byte(o.kind) == Some(AsmOperandKind::Sym) {
                o.aux += ref_off;
            }
            out.asm_operands.push(AsmOperandRec { ..o });
        }
        let clobbers_first = if rec.clobbers_len > 0 { rec.clobbers_first + asm_text_off } else { 0 };
        out.asms.push(AsmRec {
            pieces_first,
            pieces_len: rec.pieces_len,
            operands_first,
            operands_len: rec.operands_len,
            options: rec.options,
            clobbers_first,
            clobbers_len: rec.clobbers_len,
        });
    }

    Ok(ChildTables { comp_off, mem_off, sig_off, ref_off, intr_off, asm_off })
}

#[inline]
fn map_val_or_fixup<A: Allocator>(
    orig_v: u32,
    val_map: &[u32],
    fixups: &mut Vec<(usize, bool, u32), A>,
    place: usize,
    in_data: bool,
) -> u32 {
    let mapped = at_u32_or(val_map, to_idx(orig_v), NONE);
    if mapped == NONE {
        fixups.push((place, in_data, orig_v));
        0
    } else {
        mapped
    }
}

/// Copies an instruction from `outer` into `out`, remapping values and blocks.
fn emit_outer_inst<A: Allocator>(
    outer: &View<'_>,
    inst: usize,
    val_map: &[u32],
    block_map: &[u32],
    val_fixups: &mut Vec<(usize, bool, u32), A>,
    out: &mut Func<A>,
) -> u32 {
    let op = outer.op(inst);
    let op_b = at_u8(outer.ops, inst);
    let flags = at_u8(outer.flags, inst);
    let ty = at_u16(outer.tys, inst);
    let attr = at_u32(outer.attrs, inst);
    let loc = at_u32(outer.locs, inst);
    let raw_data = at_u32(outer.data, inst);
    let next_idx = out.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            map_val_or_fixup(raw_data, val_map, val_fixups, next_idx, true)
        }
        Layout::Data(Field::Block) => at_u32(block_map, to_idx(raw_data)),
        Layout::Data(_) => raw_data,
        Layout::Extra(fields) => {
            let src = to_idx(raw_data);
            let dst = to_u32(out.extra.len());
            let mut p = src;
            let mut scrutinee = None;
            for &f in fields {
                match f {
                    Field::Value => {
                        let v = at_u32(outer.extra, p);
                        scrutinee = Some(v);
                        let place = out.extra.len();
                        let mv = map_val_or_fixup(v, val_map, val_fixups, place, false);
                        out.extra.push(mv);
                        p += 1;
                    }
                    Field::Block => {
                        out.extra.push(at_u32(block_map, to_idx(at_u32(outer.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(outer.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let place = out.extra.len();
                            let mv = map_val_or_fixup(
                                at_u32(outer.extra, p + k),
                                val_map,
                                val_fixups,
                                place,
                                false,
                            );
                            out.extra.push(mv);
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at_u32(outer.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            out.extra.push(at_u32(block_map, to_idx(at_u32(outer.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        out.extra.push(at_u32(block_map, to_idx(at_u32(outer.extra, p))));
                        let n_u32 = at_u32(outer.extra, p + 1);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            let place = out.extra.len();
                            let mv = map_val_or_fixup(
                                at_u32(outer.extra, p + k),
                                val_map,
                                val_fixups,
                                place,
                                false,
                            );
                            out.extra.push(mv);
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at_u32(outer.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        let s_val = scrutinee.unwrap_or_else(|| at_u32(outer.extra, src));
                        let w = to_idx(Ty(at_u16(outer.tys, to_idx(s_val))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                out.extra.push(at_u32(outer.extra, p + k));
                            }
                            p += w;
                            out.extra.push(at_u32(block_map, to_idx(at_u32(outer.extra, p))));
                            let an_u32 = at_u32(outer.extra, p + 1);
                            let an = to_idx(an_u32);
                            out.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let place = out.extra.len();
                                let mv = map_val_or_fixup(
                                    at_u32(outer.extra, p + k),
                                    val_map,
                                    val_fixups,
                                    place,
                                    false,
                                );
                                out.extra.push(mv);
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at_u32(outer.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            out.extra.push(at_u32(outer.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            out.extra.push(at_u32(outer.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        out.extra.push(at_u32(outer.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(out.ops.len());
    out.ops.push(op_b);
    out.flags.push(flags);
    out.tys.push(ty);
    out.data.push(data);
    out.attrs.push(attr);
    out.locs.push(loc);
    idx
}

/// Synthesizes a zero/null constant of scalar type `ty` into `out` (used only when a child
/// `Op::Suspend`'s pending return signature differs from `outer`'s coroutine signature).
fn emit_default_pending_val<A: Allocator>(
    out: &mut Func<A>,
    ty: Ty,
    inst: usize,
) -> Result<u32, SimplifyError> {
    let idx = try_u32(out.ops.len(), inst, "fused coroutine ops overflow")?;
    if ty.is_scalar() && ty.is_int() {
        out.ops.push(op_byte(Op::Iconst));
        out.flags.push(0);
        out.tys.push(ty.0);
        out.data.push(0);
        out.attrs.push(0);
        out.locs.push(0);
        Ok(idx)
    } else if ty == Ty::PTR {
        out.ops.push(op_byte(Op::Null));
        out.flags.push(0);
        out.tys.push(Ty::PTR.0);
        out.data.push(0);
        out.attrs.push(0);
        out.locs.push(0);
        Ok(idx)
    } else if ty == Ty::F16 || ty == Ty::F32 {
        out.ops.push(op_byte(Op::Fconst));
        out.flags.push(0);
        out.tys.push(ty.0);
        out.data.push(0);
        out.attrs.push(0);
        out.locs.push(0);
        Ok(idx)
    } else if ty == Ty::F64 || ty == Ty::F128 {
        let dst = try_u32(out.extra.len(), inst, "fused coroutine extra overflow")?;
        let words = to_idx(ty.const_words());
        for _ in 0..words {
            out.extra.push(0);
        }
        out.ops.push(op_byte(Op::FconstWide));
        out.flags.push(0);
        out.tys.push(ty.0);
        out.data.push(dst);
        out.attrs.push(0);
        out.locs.push(0);
        Ok(idx)
    } else {
        Err(SimplifyError::Refused {
            inst,
            what: "child suspend pending value incompatible with outer coroutine return signature",
        })
    }
}

/// Copies a non-return, non-suspend instruction from `child` into `out`, remapping all tables,
/// blocks, and values.
#[allow(clippy::too_many_arguments)]
fn emit_child_inst<A: Allocator>(
    child: &View<'_>,
    inst: usize,
    tables: ChildTables,
    slot_off: u32,
    val_map: &[u32],
    block_map: &[u32],
    val_fixups: &mut Vec<(usize, bool, u32), A>,
    out: &mut Func<A>,
) -> Result<u32, SimplifyError> {
    let op = child.op(inst);
    let op_b = at_u8(child.ops, inst);
    let flags = at_u8(child.flags, inst);
    let ty = remap_child_ty(Ty(at_u16(child.tys, inst)), tables.comp_off, inst)?;
    let attr = remap_opt_row(at_u32(child.attrs, inst), tables.mem_off);
    let raw_data = at_u32(child.data, inst);
    let next_idx = out.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            map_val_or_fixup(raw_data, val_map, val_fixups, next_idx, true)
        }
        Layout::Data(Field::Block) => at_u32(block_map, to_idx(raw_data)),
        Layout::Data(Field::Ref) => raw_data + tables.ref_off,
        Layout::Data(Field::Slot) => raw_data + slot_off,
        Layout::Data(_) => raw_data,
        Layout::Extra(fields) => {
            let src = to_idx(raw_data);
            let dst = try_u32(out.extra.len(), inst, "fused coroutine extra overflow")?;
            let mut p = src;
            let mut scrutinee = None;
            for &f in fields {
                match f {
                    Field::Value => {
                        let v = at_u32(child.extra, p);
                        scrutinee = Some(v);
                        let place = out.extra.len();
                        let mv = map_val_or_fixup(v, val_map, val_fixups, place, false);
                        out.extra.push(mv);
                        p += 1;
                    }
                    Field::Block => {
                        out.extra.push(at_u32(block_map, to_idx(at_u32(child.extra, p))));
                        p += 1;
                    }
                    Field::Ref => {
                        out.extra.push(at_u32(child.extra, p) + tables.ref_off);
                        p += 1;
                    }
                    Field::Sig => {
                        out.extra.push(at_u32(child.extra, p) + tables.sig_off);
                        p += 1;
                    }
                    Field::Slot => {
                        out.extra.push(at_u32(child.extra, p) + slot_off);
                        p += 1;
                    }
                    Field::Intrinsic => {
                        out.extra.push(at_u32(child.extra, p) + tables.intr_off);
                        p += 1;
                    }
                    Field::Asm => {
                        out.extra.push(at_u32(child.extra, p) + tables.asm_off);
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(child.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let place = out.extra.len();
                            let mv = map_val_or_fixup(
                                at_u32(child.extra, p + k),
                                val_map,
                                val_fixups,
                                place,
                                false,
                            );
                            out.extra.push(mv);
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at_u32(child.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            out.extra.push(at_u32(block_map, to_idx(at_u32(child.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        out.extra.push(at_u32(block_map, to_idx(at_u32(child.extra, p))));
                        let n_u32 = at_u32(child.extra, p + 1);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            let place = out.extra.len();
                            let mv = map_val_or_fixup(
                                at_u32(child.extra, p + k),
                                val_map,
                                val_fixups,
                                place,
                                false,
                            );
                            out.extra.push(mv);
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at_u32(child.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        let s_val = scrutinee.unwrap_or_else(|| at_u32(child.extra, src));
                        let w = to_idx(Ty(at_u16(child.tys, to_idx(s_val))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                out.extra.push(at_u32(child.extra, p + k));
                            }
                            p += w;
                            out.extra.push(at_u32(block_map, to_idx(at_u32(child.extra, p))));
                            let an_u32 = at_u32(child.extra, p + 1);
                            let an = to_idx(an_u32);
                            out.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let place = out.extra.len();
                                let mv = map_val_or_fixup(
                                    at_u32(child.extra, p + k),
                                    val_map,
                                    val_fixups,
                                    place,
                                    false,
                                );
                                out.extra.push(mv);
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at_u32(child.extra, p);
                        let n = to_idx(n_u32);
                        out.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            out.extra.push(at_u32(child.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(ty.const_words());
                        for k in 0..w {
                            out.extra.push(at_u32(child.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        out.extra.push(at_u32(child.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = try_u32(out.ops.len(), inst, "fused coroutine ops overflow")?;
    out.ops.push(op_b);
    out.flags.push(flags);
    out.tys.push(ty.0);
    out.data.push(data);
    out.attrs.push(attr);
    out.locs.push(0);
    Ok(idx)
}

/// Fuses direct calls to `child_ref_id` inside `outer` by splicing `child`'s coroutine CFG
/// (including its `Op::Suspend` points) flatly into `outer`, then verifies the fused coroutine.
pub fn fuse_nested_coroutines<A: Allocator + Copy>(
    outer: View<'_>,
    child: View<'_>,
    child_ref_id: u32,
    alloc: A,
) -> Result<CoroFuseOutput<A>, SimplifyError> {
    if outer.blocks() == 0 || child.blocks() == 0 {
        return Ok(CoroFuseOutput {
            func: outer.to_func_in(alloc),
            stats: CoroFuseStats::default(),
        });
    }
    if outer.head.ptr_bits != child.head.ptr_bits {
        return Err(SimplifyError::Refused {
            inst: 0,
            what: "nested coroutine has a different pointer width than outer coroutine",
        });
    }

    // Ensure `child` has at least one `Op::Return` and has a scalar/void return matching direct calls.
    let child_has_return = (0..child.blocks())
        .any(|b| child.op(child.terminator(Block(to_u32(b)))) == Op::Return);
    if !child_has_return {
        return Ok(CoroFuseOutput {
            func: outer.to_func_in(alloc),
            stats: CoroFuseStats::default(),
        });
    }

    let child_sig = child.sig(SigId(child.head.sig));
    let child_ret_ty = match child_sig.rets {
        [] => Ty::NONE,
        [one] => Ty(one.ty),
        _ => {
            return Err(SimplifyError::Refused {
                inst: 0,
                what: "nested coroutine with multi-value compound return",
            });
        }
    };
    if child_ret_ty.compound_row().is_some() {
        return Err(SimplifyError::Refused {
            inst: 0,
            what: "nested coroutine returning a compound type",
        });
    }
    let child_params = child.params(Block(0));

    // Find all matching direct `Op::Call` sites to `child_ref_id` per outer block.
    let outer_blocks = outer.blocks();
    let mut sites_by_block: Vec<Vec<usize, A>, A> = Vec::with_capacity_in(outer_blocks, alloc);
    let mut total_sites = 0usize;
    for ob in 0..outer_blocks {
        let mut sites: Vec<usize, A> = Vec::new_in(alloc);
        for inst in outer.block(Block(to_u32(ob))) {
            if outer.op(inst) != Op::Call {
                continue;
            }
            let at = to_idx(at_u32(outer.data, inst));
            if at_u32(outer.extra, at) != child_ref_id {
                continue;
            }
            let argc = to_idx(at_u32(outer.extra, at + 2));
            if argc != child_params.len() {
                continue;
            }
            if Ty(at_u16(outer.tys, inst)) != child_ret_ty {
                continue;
            }
            let mut types_ok = true;
            for k in 0..argc {
                let arg_v = to_idx(at_u32(outer.extra, at + 3 + k));
                let p_ty = Ty(at_u16(child.tys, child_params.start + k));
                if Ty(at_u16(outer.tys, arg_v)) != p_ty || p_ty.compound_row().is_some() {
                    types_ok = false;
                    break;
                }
            }
            if types_ok {
                sites.push(inst);
                total_sites += 1;
            }
        }
        sites_by_block.push(sites);
    }

    if total_sites == 0 {
        return Ok(CoroFuseOutput {
            func: outer.to_func_in(alloc),
            stats: CoroFuseStats::default(),
        });
    }

    // Check if `child`'s entry block `0` has any internal incoming edges.
    let mut child_entry_has_preds = false;
    for cb in 0..child.blocks() {
        let term = child.terminator(Block(to_u32(cb)));
        each(&child, term, |item| {
            if let Item::Edge(edge) = item
                && edge.block == 0
            {
                child_entry_has_preds = true;
            }
        });
    }

    // Compute the first unused suspend point ID past all `Op::Suspend` points in `outer`.
    let mut next_suspend_point = 0u32;
    for inst in 0..outer.len() {
        if outer.op(inst) == Op::Suspend {
            let pt = at_u32(outer.extra, to_idx(at_u32(outer.data, inst)));
            next_suspend_point = next_suspend_point.max(pt.saturating_add(1));
        }
    }

    // Pre-assign output block numbers deterministically:
    // For each outer block `ob`, `outer_block_map[ob]` is the first segment's block ID; each
    // spliced call site inside `ob` reserves `child.blocks()` blocks followed by its continuation
    // block.
    let child_blocks_u32 = try_u32(child.blocks(), 0, "child blocks overflow u32")?;
    let mut outer_block_map = filled(outer_blocks, NONE, &alloc);
    let mut site_child_base: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(outer_blocks, alloc);
    let mut site_cont_blk: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(outer_blocks, alloc);
    let mut next_blk = 0u32;
    for ob in 0..outer_blocks {
        set_at(&mut outer_block_map, ob, next_blk);
        next_blk += 1;
        let n_sites = sites_by_block.get(ob).map_or(0, |s| s.len());
        let mut bases = Vec::with_capacity_in(n_sites, alloc);
        let mut conts = Vec::with_capacity_in(n_sites, alloc);
        for _ in 0..n_sites {
            bases.push(next_blk);
            next_blk += child_blocks_u32;
            conts.push(next_blk);
            next_blk += 1;
        }
        site_child_base.push(bases);
        site_cont_blk.push(conts);
    }

    // Clone `outer`'s tables, clear instruction/block columns, and append `child`'s shared tables.
    let mut out = outer.to_func_in(alloc);
    out.ops.clear();
    out.flags.clear();
    out.tys.clear();
    out.data.clear();
    out.extra.clear();
    out.attrs.clear();
    out.locs.clear();
    out.block_starts.clear();
    out.block_kinds.clear();

    let tables = append_child_shared_tables(&mut out, &child)?;
    let outer_sig = outer.sig(SigId(outer.head.sig));

    let mut outer_val_map = filled(outer.len(), NONE, &alloc);
    let mut outer_val_fixups: Vec<(usize, bool, u32), A> = Vec::new_in(alloc);
    let mut stats = CoroFuseStats::default();

    for ob in 0..outer_blocks {
        let range = outer.block(Block(to_u32(ob)));
        let Some(sites) = sites_by_block.get(ob) else {
            continue;
        };

        // Begin segment 0 of `ob`.
        out.block_starts.push(try_u32(out.ops.len(), range.start, "block_starts overflow")?);
        out.block_kinds.push(at_u8(outer.block_kinds, ob));

        if sites.is_empty() {
            for inst in range {
                let mapped = emit_outer_inst(
                    &outer,
                    inst,
                    &outer_val_map,
                    &outer_block_map,
                    &mut outer_val_fixups,
                    &mut out,
                );
                set_at(&mut outer_val_map, inst, mapped);
            }
            continue;
        }

        let mut cursor = range.start;
        for (s_idx, &call_inst) in sites.iter().enumerate() {
            // Emit instructions in `ob` strictly before `call_inst`.
            for inst in cursor..call_inst {
                let mapped = emit_outer_inst(
                    &outer,
                    inst,
                    &outer_val_map,
                    &outer_block_map,
                    &mut outer_val_fixups,
                    &mut out,
                );
                set_at(&mut outer_val_map, inst, mapped);
            }

            let call_at = to_idx(at_u32(outer.data, call_inst));
            let call_argc = to_idx(at_u32(outer.extra, call_at + 2));
            let call_args = req_slice(
                outer.extra,
                call_at + 3,
                call_argc,
                call_inst,
                "call arguments slice out of bounds",
            )?;
            let child_base = site_child_base
                .get(ob)
                .and_then(|v| v.get(s_idx).copied())
                .ok_or(SimplifyError::Refused {
                    inst: call_inst,
                    what: "missing child base block",
                })?;
            let cont_blk = site_cont_blk
                .get(ob)
                .and_then(|v| v.get(s_idx).copied())
                .ok_or(SimplifyError::Refused {
                    inst: call_inst,
                    what: "missing continuation block",
                })?;

            // Decide whether `child`'s entry block keeps its `Op::Param`s or binds directly.
            let keep_child_entry_params = child_entry_has_preds
                || call_args
                    .iter()
                    .any(|&a| at_u32_or(&outer_val_map, to_idx(a), NONE) == NONE);

            // End current outer segment with `Op::Jump` into `child`'s entry block.
            let jat = try_u32(out.extra.len(), call_inst, "fused extra overflow")?;
            out.extra.push(child_base);
            if keep_child_entry_params {
                out.extra.push(try_u32(call_argc, call_inst, "call_argc overflow")?);
                for &arg in call_args {
                    let place = out.extra.len();
                    let mv = map_val_or_fixup(arg, &outer_val_map, &mut outer_val_fixups, place, false);
                    out.extra.push(mv);
                }
            } else {
                out.extra.push(0);
            }
            out.ops.push(op_byte(Op::Jump));
            out.flags.push(0);
            out.tys.push(Ty::NONE.0);
            out.data.push(jat);
            out.attrs.push(0);
            out.locs.push(at_u32(outer.locs, call_inst));

            // Allocate distinct stack slots for this spliced instance of `child`.
            let slot_off = try_u32(out.slots.len(), call_inst, "slots table overflow")?;
            out.slots.extend_from_slice(child.slots);

            let mut child_block_map: Vec<u32, A> = Vec::with_capacity_in(child.blocks(), alloc);
            for cb in 0..child.blocks() {
                child_block_map.push(child_base + to_u32(cb));
            }
            let mut child_val_map = filled(child.len(), NONE, &alloc);
            let mut child_val_fixups: Vec<(usize, bool, u32), A> = Vec::new_in(alloc);

            if !keep_child_entry_params {
                for (k, &arg) in call_args.iter().enumerate() {
                    set_at(
                        &mut child_val_map,
                        child_params.start + k,
                        at_u32_or(&outer_val_map, to_idx(arg), NONE),
                    );
                }
            }

            // Emit all blocks of `child`.
            for cb in 0..child.blocks() {
                out.block_starts.push(try_u32(out.ops.len(), call_inst, "block_starts overflow")?);
                out.block_kinds.push(at_u8(child.block_kinds, cb));
                let crange = child.block(Block(to_u32(cb)));
                for c_inst in crange {
                    if cb == 0 && !keep_child_entry_params && child_params.contains(&c_inst) {
                        continue;
                    }
                    match child.op(c_inst) {
                        Op::Return => {
                            let rat = to_idx(at_u32(child.data, c_inst));
                            let rn_u32 = at_u32(child.extra, rat);
                            let rn = to_idx(rn_u32);
                            let dst = try_u32(out.extra.len(), c_inst, "fused extra overflow")?;
                            out.extra.push(cont_blk);
                            out.extra.push(rn_u32);
                            for k in 0..rn {
                                let place = out.extra.len();
                                let mv = map_val_or_fixup(
                                    at_u32(child.extra, rat + 1 + k),
                                    &child_val_map,
                                    &mut child_val_fixups,
                                    place,
                                    false,
                                );
                                out.extra.push(mv);
                            }
                            let idx = try_u32(out.ops.len(), c_inst, "fused ops overflow")?;
                            out.ops.push(op_byte(Op::Jump));
                            out.flags.push(0);
                            out.tys.push(Ty::NONE.0);
                            out.data.push(dst);
                            out.attrs.push(0);
                            out.locs.push(0);
                            set_at(&mut child_val_map, c_inst, idx);
                        }
                        Op::Suspend => {
                            let sat = to_idx(at_u32(child.data, c_inst));
                            let c_resume =
                                at_u32(&child_block_map, to_idx(at_u32(child.extra, sat + 1)));
                            let c_cancel =
                                at_u32(&child_block_map, to_idx(at_u32(child.extra, sat + 2)));
                            let pn_u32 = at_u32(child.extra, sat + 3);
                            let pn = to_idx(pn_u32);
                            let orig_pending = req_slice(
                                child.extra,
                                sat + 4,
                                pn,
                                c_inst,
                                "suspend pending values slice out of bounds",
                            )?;

                            // Check if `child`'s pending values already match `outer`'s return signature.
                            let mut pending_matches = pn == outer_sig.rets.len();
                            if pending_matches {
                                for (&pv, ret_param) in orig_pending.iter().zip(outer_sig.rets.iter())
                                {
                                    let p_ty = remap_child_ty(
                                        Ty(at_u16(child.tys, to_idx(pv))),
                                        tables.comp_off,
                                        c_inst,
                                    )?;
                                    if p_ty.0 != ret_param.ty {
                                        pending_matches = false;
                                        break;
                                    }
                                }
                            }

                            let mut synth_pending: Vec<u32, A> = Vec::new_in(alloc);
                            if !pending_matches {
                                for r in outer_sig.rets {
                                    synth_pending
                                        .push(emit_default_pending_val(&mut out, Ty(r.ty), c_inst)?);
                                }
                            }

                            let pt = next_suspend_point;
                            next_suspend_point = next_suspend_point.checked_add(1).ok_or(
                                SimplifyError::Refused {
                                    inst: c_inst,
                                    what: "fused suspend point number overflows u32",
                                },
                            )?;
                            stats.child_suspends_fused += 1;

                            let dst = try_u32(out.extra.len(), c_inst, "fused extra overflow")?;
                            out.extra.push(pt);
                            out.extra.push(c_resume);
                            out.extra.push(c_cancel);
                            if pending_matches {
                                out.extra.push(pn_u32);
                                for &pv in orig_pending {
                                    let place = out.extra.len();
                                    let mv = map_val_or_fixup(
                                        pv,
                                        &child_val_map,
                                        &mut child_val_fixups,
                                        place,
                                        false,
                                    );
                                    out.extra.push(mv);
                                }
                            } else {
                                out.extra.push(try_u32(
                                    synth_pending.len(),
                                    c_inst,
                                    "synth_pending overflow",
                                )?);
                                for &sv in &synth_pending {
                                    out.extra.push(sv);
                                }
                            }

                            let idx = try_u32(out.ops.len(), c_inst, "fused ops overflow")?;
                            out.ops.push(op_byte(Op::Suspend));
                            out.flags.push(0);
                            out.tys.push(Ty::NONE.0);
                            out.data.push(dst);
                            out.attrs.push(0);
                            out.locs.push(0);
                            set_at(&mut child_val_map, c_inst, idx);
                        }
                        _ => {
                            let mapped = emit_child_inst(
                                &child,
                                c_inst,
                                tables,
                                slot_off,
                                &child_val_map,
                                &child_block_map,
                                &mut child_val_fixups,
                                &mut out,
                            )?;
                            set_at(&mut child_val_map, c_inst, mapped);
                        }
                    }
                }
            }

            // Patch any deferred value references inside this spliced `child`.
            for &(place, in_data, orig_v) in &child_val_fixups {
                let orig_idx = to_idx(orig_v);
                let mapped = at_u32_or(&child_val_map, orig_idx, NONE);
                if mapped == NONE {
                    return Err(SimplifyError::Refused {
                        inst: orig_idx,
                        what: "unresolved child value after coroutine fusion",
                    });
                }
                if in_data {
                    set_at(&mut out.data, place, mapped);
                } else {
                    set_at(&mut out.extra, place, mapped);
                }
            }

            // Begin continuation block `cont_blk` right after `call_inst`.
            out.block_starts.push(try_u32(out.ops.len(), call_inst, "block_starts overflow")?);
            out.block_kinds.push(block_kind_byte(BlockKind::Normal));
            if !child_ret_ty.is_none() {
                let param_idx = try_u32(out.ops.len(), call_inst, "fused ops overflow")?;
                out.ops.push(op_byte(Op::Param));
                out.flags.push(0);
                out.tys.push(child_ret_ty.0);
                out.data.push(0);
                out.attrs.push(0);
                out.locs.push(at_u32(outer.locs, call_inst));
                set_at(&mut outer_val_map, call_inst, param_idx);
            }

            stats.poll_calls_eliminated += 1;
            cursor = call_inst + 1;
        }

        // Emit the tail of `ob` after the last spliced call site.
        for inst in cursor..range.end {
            let mapped = emit_outer_inst(
                &outer,
                inst,
                &outer_val_map,
                &outer_block_map,
                &mut outer_val_fixups,
                &mut out,
            );
            set_at(&mut outer_val_map, inst, mapped);
        }
    }

    // Patch any deferred value references inside `outer`.
    for &(place, in_data, orig_v) in &outer_val_fixups {
        let orig_idx = to_idx(orig_v);
        let mapped = at_u32_or(&outer_val_map, orig_idx, NONE);
        if mapped == NONE {
            return Err(SimplifyError::Refused {
                inst: orig_idx,
                what: "unresolved outer value after coroutine fusion",
            });
        }
        if in_data {
            set_at(&mut out.data, place, mapped);
        } else {
            set_at(&mut out.extra, place, mapped);
        }
    }

    verify(out.view()).map_err(SimplifyError::Verify)?;
    Ok(CoroFuseOutput { func: out, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        BlockKind, Flags, IntCond,
        builder::{Builder, Full, Param, Variadic},
        coroutine::{Shape, split},
        func::{Conv, RefKind},
    };

    #[test]
    fn frame_graph_coloring_shrinks_sequential_await_locals() {
        // 1. Always-suspending coroutine with 4 sequential `.await` (`Op::Suspend`) phases.
        //    Each phase keeps a distinct `i64` local live across ONLY its own suspend:
        //      phase 0: v0 = x + 1;  suspend 0 -> phase 1
        //      phase 1: v1 = v0 + 2; suspend 1 -> phase 2
        //      phase 2: v2 = v1 + 3; suspend 2 -> phase 3
        //      phase 3: v3 = v2 + 4; suspend 3 -> done
        //      done:    return v3
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [p0, p1, p2, p3, done, cancel] = [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(p0);
        let _state = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let pending = b.iconst(Ty::I64, 0);
        let k1 = b.iconst(Ty::I64, 1);
        let v0 = b.binary(Op::Add, Flags::NONE, x, k1);
        b.suspend(0, p1, cancel, &[pending]);

        b.begin(p1);
        let k2 = b.iconst(Ty::I64, 2);
        let v1 = b.binary(Op::Add, Flags::NONE, v0, k2);
        b.suspend(1, p2, cancel, &[pending]);

        b.begin(p2);
        let k3 = b.iconst(Ty::I64, 3);
        let v2 = b.binary(Op::Add, Flags::NONE, v1, k3);
        b.suspend(2, p3, cancel, &[pending]);

        b.begin(p3);
        let k4 = b.iconst(Ty::I64, 4);
        let v3 = b.binary(Op::Add, Flags::NONE, v2, k4);
        b.suspend(3, done, cancel, &[pending]);

        b.begin(done);
        b.ret(&[v3]);

        b.begin(cancel);
        b.cancelled();

        let seq_coro = b.finish_checked().unwrap();

        let plan = pack_coroutine_frame_colored(seq_coro.view(), 16, 8, TestHeap).unwrap();
        assert_eq!(plan.stats.saved_values, 4, "all 4 phase locals cross their respective suspends");
        assert_eq!(plan.stats.naive_bytes, 16 + 4 * 8, "naive layout allocates 4 distinct 8-byte slots");
        assert_eq!(plan.stats.colored_bytes, 16 + 8, "colored layout must pack all 4 phases into ONE 8-byte slot");
        assert!(plan.stats.colored_bytes < plan.stats.naive_bytes);
        assert_eq!(plan.total_bytes, 24);
        assert_eq!(plan.total_align, 8);
        assert!(!plan.stats.can_sync_complete, "every path suspends before returning");
        // Every one of the 4 non-overlapping phase locals shares exact offset 16!
        assert_eq!(plan.slots.len(), 4);
        for slot in &plan.slots {
            assert_eq!(slot.offset, 16, "non-interfering await locals must share the same frame offset");
            assert_eq!(slot.size, 8);
            assert_eq!(slot.align, 8);
        }

        // 2. Sync-capable coroutine: fast path returns directly without visiting `Op::Suspend`.
        let mut sb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let ssig = sb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        sb.set_signature(ssig);
        let [s_entry, s_fast, s_slow, s_resume, s_cancel] =
            [(); 5].map(|_| sb.create_block(BlockKind::Normal));

        sb.begin(s_entry);
        let _s_state = sb.param(Ty::PTR);
        let sx = sb.param(Ty::I64);
        let sz = sb.iconst(Ty::I64, 0);
        let ready = sb.icmp(IntCond::Eq, sx, sz);
        sb.br(Flags::NONE, ready, (s_fast, &[]), (s_slow, &[]));

        sb.begin(s_fast);
        let k42 = sb.iconst(Ty::I64, 42);
        sb.ret(&[k42]);

        sb.begin(s_slow);
        let k5 = sb.iconst(Ty::I64, 5);
        let sv = sb.binary(Op::Add, Flags::NONE, sx, k5);
        sb.suspend(0, s_resume, s_cancel, &[sz]);

        sb.begin(s_resume);
        sb.ret(&[sv]);

        sb.begin(s_cancel);
        sb.cancelled();

        let sync_coro = sb.finish_checked().unwrap();
        let sync_plan = pack_coroutine_frame_colored(sync_coro.view(), 0, 8, TestHeap).unwrap();
        assert!(sync_plan.stats.can_sync_complete, "fast path reaches Return with zero suspends");
        assert_eq!(sync_plan.stats.saved_values, 1);
        assert_eq!(sync_plan.stats.colored_bytes, 8);
    }

    #[test]
    fn fuse_nested_coroutines_flattens_child_suspends_into_outer() {
        // Child coroutine `child(state: ptr, x: i64) -> i64`:
        //   c0: t = x * 2; suspend 0 -> c1, c_cancel
        //   c1: r = t + 1; return r
        //   c_cancel: cancelled
        let mut cb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = cb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        cb.set_signature(sig);
        let [c0, c1, c_cancel] = [(); 3].map(|_| cb.create_block(BlockKind::Normal));
        cb.begin(c0);
        let _c_state = cb.param(Ty::PTR);
        let cx = cb.param(Ty::I64);
        let c_pend = cb.iconst(Ty::I64, 0);
        let k2 = cb.iconst(Ty::I64, 2);
        let t = cb.binary(Op::Mul, Flags::NONE, cx, k2);
        cb.suspend(0, c1, c_cancel, &[c_pend]);
        cb.begin(c1);
        let k1 = cb.iconst(Ty::I64, 1);
        let cr = cb.binary(Op::Add, Flags::NONE, t, k1);
        cb.ret(&[cr]);
        cb.begin(c_cancel);
        cb.cancelled();
        let child = cb.finish_checked().unwrap();

        // Outer coroutine `outer(state: ptr, x: i64) -> i64`:
        //   o0: v = x + 5; suspend 0 -> o1, o_cancel
        //   o1: sub = call child(state, v); out = sub + 10; return out
        //   o_cancel: cancelled
        //   o_unresumed: cancelled (unreached from entry, for `coroutine::split`)
        let mut ob = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let osig = ob.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        ob.set_signature(osig);
        let child_ref = ob.reference(RefKind::Func, 0);
        let [o0, o1, o_cancel, o_unresumed] = [(); 4].map(|_| ob.create_block(BlockKind::Normal));
        ob.begin(o0);
        let o_state = ob.param(Ty::PTR);
        let ox = ob.param(Ty::I64);
        let o_pend = ob.iconst(Ty::I64, 0);
        let k5 = ob.iconst(Ty::I64, 5);
        let ov = ob.binary(Op::Add, Flags::NONE, ox, k5);
        ob.suspend(0, o1, o_cancel, &[o_pend]);
        ob.begin(o1);
        let sub = ob.call(Flags::NONE, child_ref, osig, &[o_state, ov]);
        let k10 = ob.iconst(Ty::I64, 10);
        let final_v = ob.binary(Op::Add, Flags::NONE, sub, k10);
        ob.ret(&[final_v]);
        ob.begin(o_cancel);
        ob.cancelled();
        ob.begin(o_unresumed);
        ob.cancelled();
        let outer = ob.finish_checked().unwrap();

        let fused = fuse_nested_coroutines(outer.view(), child.view(), child_ref.0, TestHeap).unwrap();
        assert_eq!(fused.stats.child_suspends_fused, 1, "child's 1 suspend must be fused");
        assert_eq!(fused.stats.poll_calls_eliminated, 1, "call to child must be eliminated");

        let fv = fused.func.view();
        verify(fv).expect("fused flat coroutine must verify");

        // Exactly 2 `Op::Suspend`s with distinct suspend point numbers, and 0 calls to `child`!
        let mut suspend_points: Vec<u32, TestHeap> = Vec::new_in(TestHeap);
        let mut child_calls = 0usize;
        for i in 0..fv.len() {
            if fv.op(i) == Op::Suspend {
                suspend_points.push(fv.extra[fv.data[i] as usize]);
            } else if fv.op(i) == Op::Call && fv.extra[fv.data[i] as usize] == child_ref.0 {
                child_calls += 1;
            }
        }
        assert_eq!(suspend_points.len(), 2, "fused coroutine must contain 2 flat suspends");
        assert_ne!(suspend_points[0], suspend_points[1], "fused suspend point IDs must be unique");
        assert_eq!(child_calls, 0, "no direct call to child may remain");

        // Verify the fused coroutine also lowers cleanly through `crate::coroutine::split`!
        let unresumed_blk = Block((fv.blocks() - 1) as u32);
        let split_res = split(
            fv,
            Shape { state_param: 0, prefix_size: 0, prefix_align: 3, unresumed: unresumed_blk },
            TestHeap,
        )
        .expect("fused coroutine must split cleanly into poll + drop");
        assert_eq!(split_res.layout.points, 2);

        // And frame graph coloring packs the fused coroutine's sequential locals (`ov` and `t`)!
        let plan = pack_coroutine_frame_colored(fv, 0, 8, TestHeap).unwrap();
        assert!(!plan.stats.can_sync_complete);
        assert!(plan.stats.colored_bytes < plan.stats.naive_bytes, "fused sequential awaits must share frame slots");
    }
}

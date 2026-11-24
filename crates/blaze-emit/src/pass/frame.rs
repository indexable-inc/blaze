//! Frame layout for `pass.rs` (Reference tier) and shared LIR stack-slot lifetime coloring
//! (reused by `sota/isel.rs`, rule 30: one concept, one home).
//!
//! # Why this exists
//!
//! Without cell and slot coloring, a function with N LIR instructions and S stack slots gets a
//! frame of `16 * N + sum(slot.size)`: `syn::ty::parsing::ambig_ty` (~600k instructions, ~500
//! slots) produced a >9.2 MiB stack frame (overflowing macOS's 8 MiB main stack on its first
//! call) and 13.4 MiB of code because every cell past 32 KiB needed 3 instructions per access.
//!
//! # Two independent, strictly bounded, deterministic colorings
//!
//! 1. **SSA value cell coloring ([`layout_cells`])**:
//!    - Instructions with `Ty::NONE` get no cell (0 bytes).
//!    - Value-producing instructions get exact size/alignment classes: 8 bytes (8-aligned) for
//!      <=64-bit scalars, 16 bytes (16-aligned) for `i128`, `f128`, 2-word pairs, and `asm!`
//!      operands/results (which may use 16-byte `ldr q`/`str q`), and 32 bytes (16-aligned,
//!      holding the 3 words of a wide `{i128, iN}` pair).
//!    - Live intervals `[lo, hi]` (inclusive) are computed in O(N + E): block-local non-`Param`
//!      values keep exact `[v, last_use(v)]`; cross-block values and block parameters extend over
//!      every overlapping merged back-edge connected component (100% sound on arbitrary CFGs,
//!      including irreducible loops, switches, invokes, landing pads, and unreachable blocks).
//!    - Linear scan with counting-sort by `lo`, a binary min-heap of active intervals ordered by
//!      `hi`, and O(1) per-class free lists runs in O(N log K) where K is peak live values.
//!
//! 2. **LIR stack-slot lifetime coloring ([`pack_slots`])**:
//!    - Forward may-addressed dataflow over the CFG, matching LIR's `SlotLive`/`SlotDead`
//!      semantics (`blaze-lir/src/eval.rs`, `coroutine_overlap.rs`), and hardened against mid-end
//!      `SlotAddr` hoisting/GVN (`blaze-lir/src/simplify/egraph.rs`): a slot becomes live at
//!      `SlotLive` and at every non-address-arithmetic instruction that consumes a pointer
//!      derived from `SlotAddr(slot)` (`PtrAdd`/`Freeze`/`PtrWithAddr` propagate provenance), and
//!      becomes dead at `SlotDead(slot)` unless its pointer entered a block `Param` or `Select`.
//!    - Slots in disjoint `if/else` or `match` arms never conflict even without `SlotDead`, and
//!      sequential temporaries bounded by `SlotDead` share stack bytes.
//!    - Greedy placement in deterministic `(Reverse(size), Reverse(align), slot_idx)` order.

use core::alloc::Allocator;
use core::cmp::Reverse;

use blaze_lir::{
    Block, Op, Scalar, Ty, Value, View,
    func::NONE,
    op::{Layout, Word, walk},
};
use blaze_own::Vec;

use super::{FRAME_LIMIT, MOST_SLOT_ALIGN, float_width::FloatWidth, wide};
use crate::EmitError;

/// Maximum `blocks * words` bitset entries allocated during slot conflict analysis (32 MiB cap,
/// rule 18: bounded memory even on pathological inputs).
const MAX_SLOT_BITSET_WORDS: usize = 4_000_000;

#[inline]
fn idx(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[inline]
fn u32_of_u64(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[inline]
fn get_u32(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(NONE)
}

#[inline]
fn set_u32(s: &mut [u32], i: usize, v: u32) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn filled<T: Copy, A: Allocator>(len: usize, val: T, alloc: A) -> Vec<T, A> {
    let mut v = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        v.push(val);
    }
    v
}

/// Cell size/alignment class for one LIR instruction in `pass.rs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum CellClass {
    /// `Ty::NONE`: no cell allocated.
    None = 0,
    /// Single 64-bit word (8 bytes, 8-aligned).
    W8 = 1,
    /// Two 64-bit words or 16-byte vector/quad (16 bytes, 16-aligned).
    W16 = 2,
    /// Three 64-bit words for a wide `{i128, iN}` pair (32-byte stride, 16-aligned).
    W24 = 3,
}

/// Compact cell layout produced by [`layout_cells`] for `pass.rs`.
pub(crate) struct CellLayout<A: Allocator> {
    /// Per instruction, byte offset from `sp` of its cell (`NONE` for `Ty::NONE` instructions).
    pub(crate) cell_at: Vec<u32, A>,
    /// Per instruction, byte offset from `sp` of a wide pair's third word (`NONE` otherwise).
    pub(crate) wide_pair_at: Vec<u32, A>,
    /// Total bytes occupied by all cells (multiple of 8).
    pub(crate) end: u64,
}

/// Compact LIR stack-slot layout produced by [`pack_slots`].
pub(crate) struct SlotLayout<A: Allocator> {
    /// Per stack slot `0..view.slots.len()`, byte offset from `sp`.
    pub(crate) slot_at: Vec<u32, A>,
    /// Highest byte end of all placed slots (`>= base_offset`).
    pub(crate) end: u64,
    /// Maximum alignment required by any slot (at least 16).
    pub(crate) most_align: u64,
}

/// Visits every `Word` operand of instruction `inst`.
#[inline]
fn walk_operands(view: View<'_>, inst: usize, visit: &mut impl FnMut(Word)) {
    let layout = view.op(inst).info().layout;
    let words: &[u32] = match layout {
        Layout::None => &[],
        Layout::Data(_) => match view.data.get(inst) {
            Some(w) => core::slice::from_ref(w),
            None => return,
        },
        Layout::Extra(_) => {
            let Some(&start) = view.data.get(inst) else { return };
            match view.extra.get(idx(start)..) {
                Some(slice) => slice,
                None => return,
            }
        }
    };
    let Some(&ty_raw) = view.tys.get(inst) else { return };
    let _ = walk::<()>(words, layout.fields(), Ty(ty_raw), view.tys, &mut |w| {
        visit(w);
        Ok(())
    });
}

/// Returns whether `ty` is a 2-word integer/pointer pair supported by `pass.rs`.
fn is_pair(view: View<'_>, ty: Ty) -> bool {
    if ty.compound_row().is_none() {
        return false;
    }
    let members = view.members(ty);
    members.len() == 2
        && members.iter().all(|&it| {
            let t = Ty(it);
            t.is_scalar()
                && matches!(
                    t.elem(),
                    Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::Ptr
                )
        })
}

/// Computes merged connected components of all layout back-edges `(from -> to)` with `to <= from`.
///
/// Returns `(comp_id, comp_ranges)` where `comp_id[b]` is the component index covering block `b`
/// (or `NONE`), and `comp_ranges[cid]` is `(first_inst, last_inst)` inclusive.
fn back_edge_components<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> (Vec<u32, A>, Vec<(u32, u32), A>) {
    let blocks = view.blocks();
    let mut max_back_from = filled(blocks, NONE, alloc);

    for b in 0..blocks {
        let range = view.block(Block(u32_of(b)));
        if range.is_empty() {
            continue;
        }
        let term = range.end - 1;
        walk_operands(view, term, &mut |w| {
            if let Word::Block(to) = w {
                let to_usize = idx(to);
                if to_usize <= b && to_usize < blocks {
                    let cur = get_u32(&max_back_from, to_usize);
                    let b_u32 = u32_of(b);
                    if cur == NONE || b_u32 > cur {
                        set_u32(&mut max_back_from, to_usize, b_u32);
                    }
                }
            }
        });
    }

    let mut comp_id = filled(blocks, NONE, alloc);
    let mut comp_ranges: Vec<(u32, u32), A> = Vec::new_in(alloc);

    let mut cur_start: Option<usize> = None;
    let mut cur_end: usize = 0;
    for b in 0..blocks {
        let back = get_u32(&max_back_from, b);
        if back != NONE {
            let back_usize = idx(back);
            match cur_start {
                None => {
                    cur_start = Some(b);
                    cur_end = back_usize;
                }
                Some(_) => {
                    if back_usize > cur_end {
                        cur_end = back_usize;
                    }
                }
            }
        }
        if let Some(start_b) = cur_start {
            let cid = u32_of(comp_ranges.len());
            set_u32(&mut comp_id, b, cid);
            if b == cur_end {
                let first_inst = u32_of(view.block(Block(u32_of(start_b))).start);
                let last_inst = u32_of(view.block(Block(u32_of(cur_end))).end.saturating_sub(1));
                comp_ranges.push((first_inst, last_inst));
                cur_start = None;
            }
        }
    }

    (comp_id, comp_ranges)
}

/// Min-heap entry for active intervals during linear-scan cell coloring, ordered by smallest `hi`.
#[derive(Clone, Copy)]
struct ActiveCell {
    hi: u32,
    class: CellClass,
    slot: u32,
}

#[inline]
fn heap_push<A: Allocator>(heap: &mut Vec<ActiveCell, A>, item: ActiveCell) {
    heap.push(item);
    let mut i = heap.len().saturating_sub(1);
    while i > 0 {
        let parent = (i - 1) / 2;
        let (Some(p), Some(c)) = (heap.get(parent).copied(), heap.get(i).copied()) else {
            break;
        };
        if p.hi <= c.hi {
            break;
        }
        heap.swap(parent, i);
        i = parent;
    }
}

#[inline]
fn heap_pop<A: Allocator>(heap: &mut Vec<ActiveCell, A>) -> Option<ActiveCell> {
    if heap.is_empty() {
        return None;
    }
    let last = heap.len() - 1;
    heap.swap(0, last);
    let out = heap.pop()?;
    let len = heap.len();
    let mut i = 0;
    loop {
        let left = 2 * i + 1;
        if left >= len {
            break;
        }
        let right = left + 1;
        let smallest = match (heap.get(left), heap.get(right)) {
            (Some(l), Some(r)) if r.hi < l.hi => right,
            _ => left,
        };
        let (Some(cur), Some(sm)) = (heap.get(i).copied(), heap.get(smallest).copied()) else {
            break;
        };
        if cur.hi <= sm.hi {
            break;
        }
        heap.swap(i, smallest);
        i = smallest;
    }
    Some(out)
}

/// Computes compact, lifetime-colored stack cell offsets for every value-producing instruction in
/// `view` (used by `pass.rs`).
pub(crate) fn layout_cells<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<CellLayout<A>, EmitError> {
    let n = view.len();
    let blocks = view.blocks();

    let mut cell_at = filled(n, NONE, alloc);
    let mut wide_pair_at = filled(n, NONE, alloc);

    if n == 0 || blocks == 0 {
        return Ok(CellLayout { cell_at, wide_pair_at, end: 0 });
    }

    // 1. Classify every instruction and record its owning block.
    let mut class = Vec::with_capacity_in(n, alloc);
    let mut block_of = filled(n, 0u32, alloc);
    let mut is_cross = filled(n, false, alloc);
    let mut lo = Vec::with_capacity_in(n, alloc);
    let mut hi = Vec::with_capacity_in(n, alloc);

    for inst in 0..n {
        let inst_u32 = u32_of(inst);
        let ty = view.ty(Value(inst_u32));
        let c = if ty == Ty::NONE {
            CellClass::None
        } else if wide::is_wide_pair(view, ty) {
            CellClass::W24
        } else if ty == Ty::I128
            || FloatWidth::of(ty) == Some(FloatWidth::Quad)
            || is_pair(view, ty)
        {
            CellClass::W16
        } else {
            CellClass::W8
        };
        class.push(c);
        lo.push(inst_u32);
        hi.push(inst_u32);
    }

    for b in 0..blocks {
        let b_u32 = u32_of(b);
        let blk = Block(b_u32);
        let range = view.block(blk);
        for inst in range.start..range.end {
            set_u32(&mut block_of, inst, b_u32);
        }
        let params = view.params(blk);
        for p in params {
            if let Some(slot) = is_cross.get_mut(p) {
                *slot = true;
            }
            if b == 0 {
                // Entry parameters are all written in the prologue before inst 0.
                set_u32(&mut lo, p, 0);
            } else if let Some(slot) = lo.get_mut(p) {
                // Landing/merge parameters are alive at block entry.
                *slot = (*slot).min(u32_of(range.start));
            }
        }
    }

    // 2. Scan operands to compute raw `[lo, hi]`, mark cross-block uses, and upgrade `asm!`
    //    operands/results to 16-byte 16-aligned cells (for `ldr q` / `str q`).
    for b in 0..blocks {
        let b_u32 = u32_of(b);
        let blk = Block(b_u32);
        let range = view.block(blk);
        if range.is_empty() {
            continue;
        }
        let term = range.end - 1;
        for inst in range.start..range.end {
            let op = view.op(inst);
            let is_asm = op == Op::Asm;
            if is_asm && class.get(inst).copied() == Some(CellClass::W8) {
                if let Some(slot) = class.get_mut(inst) {
                    *slot = CellClass::W16;
                }
            }
            let inst_u32 = u32_of(inst);
            walk_operands(view, inst, &mut |w| match w {
                Word::Value(v) => {
                    let vi = idx(v);
                    if vi < n {
                        if let Some(slot) = lo.get_mut(vi) {
                            if inst_u32 < *slot {
                                *slot = inst_u32;
                            }
                        }
                        if let Some(slot) = hi.get_mut(vi) {
                            if inst_u32 > *slot {
                                *slot = inst_u32;
                            }
                        }
                        if get_u32(&block_of, vi) != b_u32 {
                            if let Some(slot) = is_cross.get_mut(vi) {
                                *slot = true;
                            }
                        }
                        if is_asm && class.get(vi).copied() == Some(CellClass::W8) {
                            if let Some(slot) = class.get_mut(vi) {
                                *slot = CellClass::W16;
                            }
                        }
                    }
                }
                Word::Block(to) if inst == term => {
                    let to_blk = Block(to);
                    if idx(to) < blocks && !view.block_kind(to_blk).is_landing() {
                        let term_u32 = u32_of(term);
                        for p in view.params(to_blk) {
                            if let Some(slot) = lo.get_mut(p) {
                                if term_u32 < *slot {
                                    *slot = term_u32;
                                }
                            }
                            if let Some(slot) = hi.get_mut(p) {
                                if term_u32 > *slot {
                                    *slot = term_u32;
                                }
                            }
                        }
                    }
                }
                _ => {}
            });
        }
    }

    // 3. Extend cross-block values over any overlapping merged back-edge component in O(1) each.
    let (comp_id, comp_ranges) = back_edge_components(view, alloc);
    if !comp_ranges.is_empty() {
        for v in 0..n {
            if class.get(v).copied() == Some(CellClass::None)
                || !is_cross.get(v).copied().unwrap_or(false)
            {
                continue;
            }
            let lo_v = get_u32(&lo, v);
            let lo_b = idx(get_u32(&block_of, idx(lo_v)));
            let lo_c = get_u32(&comp_id, lo_b);
            if lo_c != NONE {
                if let Some(&(c_lo, _)) = comp_ranges.get(idx(lo_c)) {
                    set_u32(&mut lo, v, lo_v.min(c_lo));
                }
            }
            let hi_v = get_u32(&hi, v);
            let hi_b = idx(get_u32(&block_of, idx(hi_v)));
            let hi_c = get_u32(&comp_id, hi_b);
            if hi_c != NONE {
                if let Some(&(_, c_hi)) = comp_ranges.get(idx(hi_c)) {
                    set_u32(&mut hi, v, hi_v.max(c_hi));
                }
            }
        }
    }

    // 4. Counting-sort all valued instructions by ascending `lo` in O(N).
    let mut bucket_head = filled(n, NONE, alloc);
    let mut bucket_next = filled(n, NONE, alloc);
    // Iterate in reverse so each bucket list comes out in ascending `v` order (deterministic).
    for v in (0..n).rev() {
        if class.get(v).copied() == Some(CellClass::None) {
            continue;
        }
        let start = idx(get_u32(&lo, v));
        let prev = get_u32(&bucket_head, start);
        set_u32(&mut bucket_next, v, prev);
        set_u32(&mut bucket_head, start, u32_of(v));
    }

    // 5. Linear scan with binary min-heap on `hi` and O(1) per-class free lists.
    let mut assigned = filled(n, NONE, alloc);
    let mut active: Vec<ActiveCell, A> = Vec::new_in(alloc);
    let mut free_w8: Vec<u32, A> = Vec::new_in(alloc);
    let mut free_w16: Vec<u32, A> = Vec::new_in(alloc);
    let mut free_w24: Vec<u32, A> = Vec::new_in(alloc);
    let mut count_w8: u32 = 0;
    let mut count_w16: u32 = 0;
    let mut count_w24: u32 = 0;

    for pos in 0..n {
        let pos_u32 = u32_of(pos);
        // Release intervals that ended strictly before `pos` (inclusive interference: `hi < pos`).
        while let Some(top) = active.first().copied() {
            if top.hi >= pos_u32 {
                break;
            }
            let Some(done) = heap_pop(&mut active) else { break };
            match done.class {
                CellClass::W8 => free_w8.push(done.slot),
                CellClass::W16 => free_w16.push(done.slot),
                CellClass::W24 => free_w24.push(done.slot),
                CellClass::None => {}
            }
        }

        let mut cur = get_u32(&bucket_head, pos);
        while cur != NONE {
            let v = idx(cur);
            cur = get_u32(&bucket_next, v);
            let c = class.get(v).copied().unwrap_or(CellClass::None);
            let slot = match c {
                CellClass::W8 => free_w8.pop().unwrap_or_else(|| {
                    let s = count_w8;
                    count_w8 += 1;
                    s
                }),
                CellClass::W16 => free_w16.pop().unwrap_or_else(|| {
                    let s = count_w16;
                    count_w16 += 1;
                    s
                }),
                CellClass::W24 => free_w24.pop().unwrap_or_else(|| {
                    let s = count_w24;
                    count_w24 += 1;
                    s
                }),
                CellClass::None => continue,
            };
            set_u32(&mut assigned, v, slot);
            heap_push(&mut active, ActiveCell { hi: get_u32(&hi, v), class: c, slot });
        }
    }

    // 6. Map per-class indices to exact aligned byte offsets:
    //    [W16 (16-aligned)] [W24 (16-aligned, 32B stride)] [W8 (8-aligned)].
    let base_w16: u64 = 0;
    let base_w24: u64 = base_w16 + u64::from(count_w16) * 16;
    let base_w8: u64 = base_w24 + u64::from(count_w24) * 32;
    let end: u64 = base_w8 + u64::from(count_w8) * 8;
    if end >= FRAME_LIMIT {
        return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
    }

    for v in 0..n {
        let slot = get_u32(&assigned, v);
        match class.get(v).copied().unwrap_or(CellClass::None) {
            CellClass::None => {}
            CellClass::W8 => {
                set_u32(&mut cell_at, v, u32_of_u64(base_w8 + u64::from(slot) * 8));
            }
            CellClass::W16 => {
                set_u32(&mut cell_at, v, u32_of_u64(base_w16 + u64::from(slot) * 16));
            }
            CellClass::W24 => {
                let off = u32_of_u64(base_w24 + u64::from(slot) * 32);
                set_u32(&mut cell_at, v, off);
                set_u32(&mut wide_pair_at, v, off + 16);
            }
        }
    }

    Ok(CellLayout { cell_at, wide_pair_at, end })
}

/// Calls `f` with each set bit index in `bits`, in ascending order.
#[inline]
fn for_each_bit(bits: &[u64], mut f: impl FnMut(usize)) {
    for (w, &word) in bits.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            f(w * 64 + idx(rest.trailing_zeros()));
            rest &= rest - 1;
        }
    }
}

/// Computes reverse postorder of reachable blocks (followed by any unreachable blocks).
fn block_rpo<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Vec<u32, A> {
    let blocks = view.blocks();

    // Build flat successor CSR for fast DFS.
    let mut succ_start = Vec::with_capacity_in(blocks + 1, alloc);
    let mut succs: Vec<u32, A> = Vec::new_in(alloc);
    for b in 0..blocks {
        succ_start.push(u32_of(succs.len()));
        let range = view.block(Block(u32_of(b)));
        if range.is_empty() {
            continue;
        }
        let term = range.end - 1;
        walk_operands(view, term, &mut |w| {
            if let Word::Block(to) = w {
                if idx(to) < blocks {
                    succs.push(to);
                }
            }
        });
    }
    succ_start.push(u32_of(succs.len()));

    let mut visited = filled(blocks, false, alloc);
    let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    let mut stack: Vec<(u32, u32), A> = Vec::new_in(alloc);

    if blocks > 0 {
        if let Some(v0) = visited.get_mut(0) {
            *v0 = true;
        }
        stack.push((0, get_u32(&succ_start, 0)));
        while let Some(&mut (b, ref mut next)) = stack.last_mut() {
            let end = get_u32(&succ_start, idx(b) + 1);
            if *next < end {
                let to = get_u32(&succs, idx(*next));
                *next += 1;
                if let Some(vis) = visited.get_mut(idx(to)) {
                    if !*vis {
                        *vis = true;
                        stack.push((to, get_u32(&succ_start, idx(to))));
                    }
                }
            } else {
                stack.pop();
                post.push(b);
            }
        }
    }

    post.reverse();
    // Append any unreachable blocks in deterministic order so all blocks are processed.
    for (b, &vis) in visited.iter().enumerate() {
        if !vis {
            post.push(u32_of(b));
        }
    }

    post
}

/// Packs LIR stack slots starting at `base_offset` using sound forward may-addressed lifetime
/// coloring (`SlotLive`, `SlotDead`, and provenance-tracked `SlotAddr` uses).
///
/// Reused by both `pass.rs` (Reference tier) and `sota/isel.rs` (SOTA tier).
pub(crate) fn pack_slots<A: Allocator + Copy>(
    view: View<'_>,
    base_offset: u64,
    alloc: A,
) -> Result<SlotLayout<A>, EmitError> {
    let slot_count = view.slots.len();
    let mut most_align: u64 = 16;
    for slot in view.slots {
        if slot.align > MOST_SLOT_ALIGN {
            return Err(EmitError::TooLarge { what: "a stack slot's alignment (past 4 KiB)" });
        }
        most_align = most_align.max(1u64 << slot.align);
    }

    let mut slot_at = filled(slot_count, 0u32, alloc);

    if slot_count == 0 {
        return Ok(SlotLayout { slot_at, end: base_offset, most_align });
    }
    if let Some(&slot) = view.slots.first() {
        if slot_count == 1 {
            let align = 1u64 << slot.align;
            let off = base_offset.next_multiple_of(align);
            let end = off + u64::from(slot.size);
            if end >= FRAME_LIMIT {
                return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
            }
            set_u32(&mut slot_at, 0, u32_of_u64(off));
            return Ok(SlotLayout { slot_at, end, most_align });
        }
    }

    let blocks = view.blocks();
    let n = view.len();

    // 1. Select tracked non-zero slots within the bounded bitset memory budget.
    let mut nonzero: Vec<u32, A> = Vec::with_capacity_in(slot_count, alloc);
    for (i, s) in view.slots.iter().enumerate() {
        if s.size > 0 {
            nonzero.push(u32_of(i));
        }
    }
    nonzero.sort_unstable_by_key(|&s| {
        let (size, align) = view.slots.get(idx(s)).map_or((0, 0), |rec| (rec.size, rec.align));
        (Reverse(size), Reverse(align), s)
    });

    let max_words = (MAX_SLOT_BITSET_WORDS / blocks.max(1)).max(1);
    let max_tracked = max_words.saturating_mul(64).min(nonzero.len());
    let tracked = nonzero.get(..max_tracked).unwrap_or(&[]);
    let untracked = nonzero.get(max_tracked..).unwrap_or(&[]);
    let tracked_count = tracked.len();
    let words = tracked_count.div_ceil(64);

    if tracked_count <= 1 || blocks == 0 {
        let mut cursor = base_offset;
        for (i, slot) in view.slots.iter().enumerate() {
            let align = 1u64 << slot.align;
            cursor = cursor.next_multiple_of(align);
            if cursor >= FRAME_LIMIT {
                return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
            }
            set_u32(&mut slot_at, i, u32_of_u64(cursor));
            cursor += u64::from(slot.size);
            if cursor >= FRAME_LIMIT {
                return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
            }
        }
        return Ok(SlotLayout { slot_at, end: cursor, most_align });
    }

    let mut bit_of = filled(slot_count, NONE, alloc);
    for (bit, &s) in tracked.iter().enumerate() {
        set_u32(&mut bit_of, idx(s), u32_of(bit));
    }

    // 2. Compute RPO, then propagate single-slot pointer provenance
    //    (`SlotAddr` -> `PtrAdd` / `Freeze` / `PtrWithAddr`) in RPO order.
    let rpo = block_rpo(view, alloc);
    let mut val_bit = filled(n, NONE, alloc);

    for &b_u32 in rpo.iter() {
        let range = view.block(Block(b_u32));
        for inst in range.start..range.end {
            let op = view.op(inst);
            match op {
                Op::SlotAddr => {
                    if let Some(&s_raw) = view.data.get(inst) {
                        let s = idx(s_raw);
                        if s < slot_count {
                            set_u32(&mut val_bit, inst, get_u32(&bit_of, s));
                        }
                    }
                }
                Op::PtrAdd | Op::Freeze | Op::PtrWithAddr => {
                    let mut first_val = NONE;
                    walk_operands(view, inst, &mut |w| {
                        if first_val == NONE {
                            if let Word::Value(v) = w {
                                first_val = v;
                            }
                        }
                    });
                    let fv = idx(first_val);
                    if fv < n {
                        let inherited = get_u32(&val_bit, fv);
                        set_u32(&mut val_bit, inst, inherited);
                    }
                }
                _ => {}
            }
        }
    }

    // 3. Collect per-block slot live/dead events in block index order.
    let mut never_kill = filled(tracked_count, false, alloc);
    let mut event_start = Vec::with_capacity_in(blocks + 1, alloc);
    let mut events: Vec<u32, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        event_start.push(u32_of(events.len()));
        let range = view.block(Block(u32_of(b)));
        for inst in range.start..range.end {
            let op = view.op(inst);
            match op {
                Op::SlotLive => {
                    if let Some(&s_raw) = view.data.get(inst) {
                        let bit = get_u32(&bit_of, idx(s_raw));
                        if bit != NONE {
                            events.push(bit << 1);
                        }
                    }
                }
                Op::SlotDead => {
                    if let Some(&s_raw) = view.data.get(inst) {
                        let bit = get_u32(&bit_of, idx(s_raw));
                        if bit != NONE {
                            events.push((bit << 1) | 1);
                        }
                    }
                }
                Op::SlotAddr | Op::PtrAdd | Op::Freeze | Op::PtrWithAddr => {}
                _ => {
                    let escapes_to_phi_or_select = op == Op::Select || op.is_terminator();
                    walk_operands(view, inst, &mut |w| {
                        if let Word::Value(v) = w {
                            let vi = idx(v);
                            if vi < n {
                                let bit = get_u32(&val_bit, vi);
                                if bit != NONE {
                                    events.push(bit << 1);
                                    if escapes_to_phi_or_select {
                                        if let Some(nk) = never_kill.get_mut(idx(bit)) {
                                            *nk = true;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    }
    event_start.push(u32_of(events.len()));

    // 4. Forward may-addressed fixed-point dataflow in RPO worklist order.
    let mut at_start = filled(blocks * words, 0u64, alloc);
    let mut cur = filled(words, 0u64, alloc);

    let mut queued = filled(blocks, true, alloc);
    let mut work = Vec::with_capacity_in(blocks, alloc);
    // Push reverse RPO so pop() visits blocks in RPO order (1 pass on DAGs).
    for &b in rpo.iter().rev() {
        work.push(b);
    }

    while let Some(b_u32) = work.pop() {
        let b = idx(b_u32);
        if let Some(q) = queued.get_mut(b) {
            *q = false;
        }
        let base = b * words;
        if let Some(src) = at_start.get(base..base + words) {
            cur.copy_from_slice(src);
        }

        let ev_s = idx(get_u32(&event_start, b));
        let ev_e = idx(get_u32(&event_start, b + 1));
        if let Some(ev_slice) = events.get(ev_s..ev_e) {
            for &ev in ev_slice {
                let bit = idx(ev >> 1);
                let is_dead = (ev & 1) != 0;
                let w = bit / 64;
                let m = 1u64 << (bit % 64);
                if let Some(cw) = cur.get_mut(w) {
                    if is_dead {
                        if !never_kill.get(bit).copied().unwrap_or(false) {
                            *cw &= !m;
                        }
                    } else {
                        *cw |= m;
                    }
                }
            }
        }

        let range = view.block(Block(b_u32));
        if range.is_empty() {
            continue;
        }
        let term = range.end - 1;
        walk_operands(view, term, &mut |w| {
            if let Word::Block(to) = w {
                let to_usize = idx(to);
                if to_usize < blocks {
                    let to_base = to_usize * words;
                    let mut grew = false;
                    if let Some(dst_slice) = at_start.get_mut(to_base..to_base + words) {
                        for (dst, &out) in dst_slice.iter_mut().zip(cur.iter()) {
                            if (out & !*dst) != 0 {
                                *dst |= out;
                                grew = true;
                            }
                        }
                    }
                    if grew {
                        if let Some(q) = queued.get_mut(to_usize) {
                            if !*q {
                                *q = true;
                                work.push(to);
                            }
                        }
                    }
                }
            }
        });
    }

    // 5. Build symmetric conflict matrix over tracked slots.
    //    Soundness theorem: along any dynamic path from entry (empty live set) where two slots
    //    `i` and `j` are simultaneously live, one must become `Live` while the other is already
    //    live (and thus present in the forward may-live set `cur`). Recording conflicts on EVERY
    //    `Live(bit)` event (even when `bit` is already in `cur` via a loop back-edge) captures
    //    every true conflict with ZERO false merge-block conflicts between disjoint `if/else` arms.
    let mut conflict = filled(tracked_count * words, 0u64, alloc);

    for b in 0..blocks {
        let base = b * words;
        let ev_s = idx(get_u32(&event_start, b));
        let ev_e = idx(get_u32(&event_start, b + 1));
        if ev_s == ev_e {
            continue;
        }
        if let Some(src) = at_start.get(base..base + words) {
            cur.copy_from_slice(src);
        }
        if let Some(ev_slice) = events.get(ev_s..ev_e) {
            for &ev in ev_slice {
                let bit = idx(ev >> 1);
                let is_dead = (ev & 1) != 0;
                let w = bit / 64;
                let m = 1u64 << (bit % 64);
                if is_dead {
                    if !never_kill.get(bit).copied().unwrap_or(false) {
                        if let Some(cw) = cur.get_mut(w) {
                            *cw &= !m;
                        }
                    }
                } else {
                    if let Some(row) = conflict.get_mut(bit * words..(bit + 1) * words) {
                        for (r, &c) in row.iter_mut().zip(cur.iter()) {
                            *r |= c;
                        }
                    }
                    for_each_bit(&cur, |other| {
                        if let Some(cell) = conflict.get_mut(other * words + w) {
                            *cell |= m;
                        }
                    });
                    if let Some(cw) = cur.get_mut(w) {
                        *cw |= m;
                    }
                }
            }
        }
    }

    // 6. Greedy deterministic placement (`tracked` is already sorted by descending size,
    //    descending align, ascending slot index).
    let mut placed_start = filled(tracked_count, 0u64, alloc);
    let mut placed_end = filled(tracked_count, 0u64, alloc);
    let mut max_end = base_offset;

    for (t, &s_u32) in tracked.iter().enumerate() {
        let s = idx(s_u32);
        let Some(&rec) = view.slots.get(s) else { continue };
        let align = 1u64 << rec.align;
        let size = u64::from(rec.size);
        let row = conflict.get(t * words..(t + 1) * words).unwrap_or(&[]);

        let mut off = base_offset.next_multiple_of(align);
        loop {
            let end = off + size;
            let mut next_off = None;
            // Check against all earlier-placed conflicting slots (`u < t`).
            for (wi, &word) in row.iter().enumerate() {
                let mut mask = word;
                while mask != 0 {
                    let u = wi * 64 + idx(mask.trailing_zeros());
                    mask &= mask - 1;
                    if u >= t {
                        break;
                    }
                    let u_s = placed_start.get(u).copied().unwrap_or(0);
                    let u_e = placed_end.get(u).copied().unwrap_or(0);
                    if off < u_e && u_s < end {
                        let cand = u_e.next_multiple_of(align);
                        next_off = Some(match next_off {
                            Some(prev) if prev >= cand => prev,
                            _ => cand,
                        });
                    }
                }
            }
            match next_off {
                Some(n_off) => off = n_off,
                None => break,
            }
        }

        let end = off + size;
        if end >= FRAME_LIMIT {
            return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
        }
        set_u32(&mut slot_at, s, u32_of_u64(off));
        if let Some(ps) = placed_start.get_mut(t) {
            *ps = off;
        }
        if let Some(pe) = placed_end.get_mut(t) {
            *pe = end;
        }
        if end > max_end {
            max_end = end;
        }
    }

    // Place zero-sized slots at aligned `base_offset`.
    for (i, rec) in view.slots.iter().enumerate() {
        if rec.size == 0 {
            let align = 1u64 << rec.align;
            let off = base_offset.next_multiple_of(align);
            if off >= FRAME_LIMIT {
                return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
            }
            set_u32(&mut slot_at, i, u32_of_u64(off));
            if off > max_end {
                max_end = off;
            }
        }
    }

    // Place any untracked non-zero slots disjointly after `max_end`.
    for &s_u32 in untracked {
        let s = idx(s_u32);
        let Some(&rec) = view.slots.get(s) else { continue };
        let align = 1u64 << rec.align;
        let off = max_end.next_multiple_of(align);
        let end = off + u64::from(rec.size);
        if end >= FRAME_LIMIT {
            return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
        }
        set_u32(&mut slot_at, s, u32_of_u64(off));
        max_end = end;
    }

    Ok(SlotLayout { slot_at, end: max_end, most_align })
}

/// Precomputes in one O(B) pass, for every block `b`, the `Op::Switch` terminator instruction
/// whose empty-arg default edge is the SOLE incoming edge to `b` (`NONE` otherwise).
///
/// Matches `blaze_lir::verify::sole_switch_default` identically, avoiding an O(B) scan per
/// `Op::Unreachable` / `Op::Switch` instruction (which becomes O(B^2) on large match-heavy functions).
pub(crate) fn sole_switch_defaults<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Vec<u32, A> {
    let blocks = view.blocks();
    let mut in_edges = filled(blocks, 0u32, alloc);
    let mut switch_term = filled(blocks, NONE, alloc);

    for b in 0..blocks {
        let range = view.block(Block(u32_of(b)));
        if range.is_empty() {
            continue;
        }
        let term = range.end - 1;
        walk_operands(view, term, &mut |w| {
            if let Word::Block(to) = w {
                if let Some(cnt) = in_edges.get_mut(idx(to)) {
                    *cnt = cnt.saturating_add(1);
                }
            }
        });
        if view.op(term) == Op::Switch {
            let mut o = view.operands(term);
            let _ = o.value();
            let default = o.target();
            if default.args.is_empty() && default.block.index() < blocks {
                set_u32(&mut switch_term, default.block.index(), u32_of(term));
            }
        }
    }

    for (st, &cnt) in switch_term.iter_mut().zip(in_edges.iter()) {
        if cnt != 1 {
            *st = NONE;
        }
    }
    switch_term
}

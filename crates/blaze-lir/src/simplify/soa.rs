//! Compiler-Driven Automatic `AoS -> SoA` Layout Transformation & Hot/Cold Field Splitting
//! (`blaze_lir::soa`).
//!
//! Analyzes counted loops streaming over an Array-of-Structs (`ptr[i * stride + field_off]`),
//! partitions struct fields into **hot** (accessed inside the loop) and **cold** (untouched)
//! subsets, and rewrites the function to consume contiguous Structure-of-Arrays column pointers
//! (`col_0_ptr, ..., col_{K-1}_ptr`) with unit-stride element loads (`col_k[i]`).
//!
//! Benefits:
//! - Eliminates cache-line waste from cold fields (e.g., 2 hot `i64` fields in a 64-byte struct
//!   -> 75% memory bandwidth saved, 4x effective L1/L2 cache capacity).
//! - Turns strided gathers (`ptr + i * 64 + 32`) into contiguous unit-stride loads
//!   (`col_k + (i << 3)`), immediately unlocking 4x-unrolled 128-bit/256-bit SIMD
//!   auto-vectorization (`crate::vectorize::auto_vectorize_loops`)!
//!
//! Pure sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit
//! allocator (`blaze_own::Vec<T, A>`) only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, dead, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::{Ext, MemAttrsRec, ParamKind, ParamRec, SigId, SigRec},
    op::{Field, Layout},
    verify,
};

/// Maximum recursion depth for affine address matching (Rule 18: bounded progress).
const MAX_ADDR_DEPTH: u32 = 8;

/// Description of one field inside an AoS struct layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StructField {
    /// Byte offset of this field from the start of the struct.
    pub offset: u32,
    /// Size of this field in bytes.
    pub size: u32,
    /// LIR scalar type loaded for this field.
    pub ty: Ty,
}

/// Hot/cold field partition and bandwidth savings discovered by [`analyze_aos_loop`].
#[derive(PartialEq, Eq, Debug)]
pub struct SoaAnalysis<A: Allocator> {
    /// Stride in bytes between consecutive AoS struct elements.
    pub struct_stride: u64,
    /// Sorted indices into `fields` that are accessed inside counted loop(s).
    pub hot_field_indices: Vec<usize, A>,
    /// Sorted indices into `fields` that are never accessed inside counted loop(s).
    pub cold_field_indices: Vec<usize, A>,
    /// Total bytes per element touched by hot fields.
    pub hot_bytes: u64,
    /// Exact integer percentage of memory bandwidth saved:
    /// `((struct_stride - hot_bytes) * 100 / struct_stride) as u32`.
    pub bandwidth_saved_pct: u32,
}

/// Telemetry returned by [`rewrite_aos_to_soa`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SoaStats {
    /// Number of strided AoS `Op::Load` instructions rewritten into contiguous column loads.
    pub loads_rewritten: u32,
    /// Number of contiguous SoA column pointer parameters created (`hot_field_indices.len()`).
    pub columns_created: u32,
    /// Exact integer percentage of memory bandwidth saved by dropping cold fields.
    pub bandwidth_saved_pct: u32,
}

/// Output of [`rewrite_aos_to_soa`].
#[derive(Debug)]
pub struct SoaOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SoaStats,
}

/// A counted natural loop with unit-step induction variable `iv_inst`.
struct CountedLoop<A: Allocator> {
    iv_inst: u32,
    blocks: Vec<u32, A>,
}

/// One matched strided AoS load inside a counted loop.
#[derive(Clone, Copy, Debug)]
struct AosLoadSite {
    load_inst: usize,
    iv_inst: u32,
    field_idx: usize,
}

struct DetailedSoaPlan<A: Allocator> {
    analysis: SoaAnalysis<A>,
    ptr_inst: u32,
    loads: Vec<AosLoadSite, A>,
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
const fn ext_byte(e: Ext) -> u8 {
    match e {
        Ext::None => 0,
        Ext::Zext => 1,
        Ext::Sext => 2,
    }
}

#[inline]
const fn param_kind_byte(k: ParamKind) -> u8 {
    match k {
        ParamKind::Value => 0,
        ParamKind::Sret => 1,
        ParamKind::Byval => 2,
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

enum IvFrame {
    Eval { val: u32, depth: u32 },
    CombineMul,
    CombineShl(u64),
    CombineAdd,
    CombineSub,
    CombineOr,
}

/// Decomposes an integer expression into `(iv_scale, const_offset)` representing
/// `iv_inst * iv_scale + const_offset`, bounded by `MAX_ADDR_DEPTH`.
fn decompose_iv_int<A: Allocator + Copy>(
    view: &View<'_>,
    val: u32,
    iv_inst: u32,
    start_depth: u32,
    alloc: A,
) -> Option<(u64, i64)> {
    let mut frames: Vec<IvFrame, A> = Vec::with_capacity_in(16, alloc);
    let mut vals: Vec<(u64, i64), A> = Vec::with_capacity_in(16, alloc);
    frames.push(IvFrame::Eval { val, depth: start_depth });

    while let Some(frame) = frames.pop() {
        match frame {
            IvFrame::Eval { val: cur, depth } => {
                if depth >= MAX_ADDR_DEPTH {
                    return None;
                }
                if cur == iv_inst {
                    vals.push((1, 0));
                    continue;
                }
                let vi = to_idx(cur);
                if let Some(c) = const_u64(view, vi) {
                    let ci = i64::try_from(c).ok()?;
                    vals.push((0, ci));
                    continue;
                }
                match view.op(vi) {
                    Op::Zext | Op::Sext => {
                        frames.push(IvFrame::Eval { val: at(view.data, vi), depth: depth + 1 });
                    }
                    Op::Mul => {
                        let idx = to_idx(at(view.data, vi));
                        let (l, r) = (at(view.extra, idx), at(view.extra, idx + 1));
                        frames.push(IvFrame::CombineMul);
                        frames.push(IvFrame::Eval { val: r, depth: depth + 1 });
                        frames.push(IvFrame::Eval { val: l, depth: depth + 1 });
                    }
                    Op::Shl => {
                        let idx = to_idx(at(view.data, vi));
                        let (l, r) = (at(view.extra, idx), at(view.extra, idx + 1));
                        let shift = const_u64(view, to_idx(r))?;
                        if shift >= 63 {
                            return None;
                        }
                        frames.push(IvFrame::CombineShl(1u64 << shift));
                        frames.push(IvFrame::Eval { val: l, depth: depth + 1 });
                    }
                    Op::Add => {
                        let idx = to_idx(at(view.data, vi));
                        let (l, r) = (at(view.extra, idx), at(view.extra, idx + 1));
                        frames.push(IvFrame::CombineAdd);
                        frames.push(IvFrame::Eval { val: r, depth: depth + 1 });
                        frames.push(IvFrame::Eval { val: l, depth: depth + 1 });
                    }
                    Op::Sub => {
                        let idx = to_idx(at(view.data, vi));
                        let (l, r) = (at(view.extra, idx), at(view.extra, idx + 1));
                        frames.push(IvFrame::CombineSub);
                        frames.push(IvFrame::Eval { val: r, depth: depth + 1 });
                        frames.push(IvFrame::Eval { val: l, depth: depth + 1 });
                    }
                    Op::Or => {
                        // Canonicalized disjoint-bit add: `Or(iv * 2^k, field_off)` where `0 <= field_off < 2^k`.
                        let idx = to_idx(at(view.data, vi));
                        let (l, r) = (at(view.extra, idx), at(view.extra, idx + 1));
                        frames.push(IvFrame::CombineOr);
                        frames.push(IvFrame::Eval { val: r, depth: depth + 1 });
                        frames.push(IvFrame::Eval { val: l, depth: depth + 1 });
                    }
                    _ => return None,
                }
            }
            IvFrame::CombineMul => {
                let (sr, cr) = vals.pop()?;
                let (sl, cl) = vals.pop()?;
                if sr == 0 && cr >= 0 {
                    let factor = cr.cast_unsigned();
                    vals.push((sl.checked_mul(factor)?, cl.checked_mul(cr)?));
                } else if sl == 0 && cl >= 0 {
                    let factor = cl.cast_unsigned();
                    vals.push((sr.checked_mul(factor)?, cr.checked_mul(cl)?));
                } else {
                    return None;
                }
            }
            IvFrame::CombineShl(factor) => {
                let (sl, cl) = vals.pop()?;
                vals.push((sl.checked_mul(factor)?, cl.checked_mul(factor.cast_signed())?));
            }
            IvFrame::CombineAdd => {
                let (sr, cr) = vals.pop()?;
                let (sl, cl) = vals.pop()?;
                vals.push((sl.checked_add(sr)?, cl.checked_add(cr)?));
            }
            IvFrame::CombineSub => {
                let (sr, cr) = vals.pop()?;
                let (sl, cl) = vals.pop()?;
                if sr != 0 {
                    return None;
                }
                vals.push((sl, cl.checked_sub(cr)?));
            }
            IvFrame::CombineOr => {
                let (sr, cr) = vals.pop()?;
                let (sl, cl) = vals.pop()?;
                if sl > 0 && sl.is_power_of_two() && cl == 0 && sr == 0 && cr >= 0 && cr.cast_unsigned() < sl {
                    vals.push((sl, cr));
                } else if sr > 0 && sr.is_power_of_two() && cr == 0 && sl == 0 && cl >= 0 && cl.cast_unsigned() < sr {
                    vals.push((sr, cl));
                } else {
                    return None;
                }
            }
        }
    }

    vals.pop()
}

/// Decomposes a pointer expression + load offset into `(iv_scale, total_const_off)` relative to
/// `ptr_inst`, representing `ptr_inst + iv_inst * iv_scale + total_const_off`.
fn decompose_aos_ptr<A: Allocator + Copy>(
    view: &View<'_>,
    ptr_val: u32,
    extra_off: i32,
    ptr_inst: u32,
    iv_inst: u32,
    alloc: A,
) -> Option<(u64, i64)> {
    let mut cur = ptr_val;
    let mut scale = 0u64;
    let mut off = i64::from(extra_off);
    for depth in 0..MAX_ADDR_DEPTH {
        if cur == ptr_inst {
            return Some((scale, off));
        }
        let pi = to_idx(cur);
        if view.op(pi) != Op::PtrAdd {
            return None;
        }
        let idx = to_idx(at(view.data, pi));
        let base = at(view.extra, idx);
        let by = at(view.extra, idx + 1);
        let (s_by, c_by) = decompose_iv_int(view, by, iv_inst, depth + 1, alloc)?;
        scale = scale.checked_add(s_by)?;
        off = off.checked_add(c_by)?;
        cur = base;
    }
    None
}

/// Finds all counted natural loops in `view` with a unit-step (`+1`) induction variable.
fn find_counted_loops<A: Allocator + Copy>(
    view: &View<'_>,
    inst_block: &[u32],
    alloc: &A,
) -> Vec<CountedLoop<A>, A> {
    let blocks = view.blocks();
    let mut loops: Vec<CountedLoop<A>, A> = Vec::new_in(*alloc);
    if blocks < 2 {
        return loops;
    }

    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
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

    let mut in_loop = filled(blocks, false, alloc);
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let mut latches: Vec<u32, A> = Vec::new_in(*alloc);
        let mut pre_count = 0u32;
        for &p in preds.get(h).map(Vec::as_slice).unwrap_or(&[]) {
            if p >= header {
                latches.push(p);
            } else {
                pre_count += 1;
            }
        }
        if latches.len() != 1 || pre_count != 1 {
            continue;
        }
        let latch = latches[0];
        let h_params = view.params(Block(header));
        if h_params.is_empty() {
            continue;
        }

        // Collect all blocks in this natural loop via backward walk from `latch` stopping at `header`.
        let mut loop_blocks: Vec<u32, A> = Vec::new_in(*alloc);
        let mut work: Vec<u32, A> = Vec::new_in(*alloc);
        set_at(&mut in_loop, h, true);
        loop_blocks.push(header);
        if latch != header {
            set_at(&mut in_loop, to_idx(latch), true);
            loop_blocks.push(latch);
            work.push(latch);
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

        // Find the backedge argument list passed from `latch` to `header`.
        let latch_term = view.terminator(Block(latch));
        let mut back_args_start: Option<usize> = None;
        each(view, latch_term, |item| {
            if let Item::Edge(edge) = item
                && edge.block == header
                && let Some((start, argc)) = edge.args
                && argc == h_params.len()
            {
                back_args_start = Some(start);
            }
        });

        let mut found_iv: Option<u32> = None;
        if !malformed
            && let Some(b_start) = back_args_start
        {
            // Find a header parameter `iv` incremented by 1 (`iv_next = Add(iv, 1)`) and
            // tested against a loop-invariant bound in a loop exit `Br`.
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at(view.tys, p_idx));
                if p_ty != Ty::I64 && p_ty != Ty::I32 {
                    continue;
                }
                let back_v = at(view.extra, b_start + pos);
                let bi = to_idx(back_v);
                if view.op(bi) != Op::Add || !at(&in_loop, to_idx(at(inst_block, bi))) {
                    continue;
                }
                let aat = to_idx(at(view.data, bi));
                let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
                let step1 = (al == p_inst && const_u64(view, to_idx(ar)) == Some(1))
                    || (ar == p_inst && const_u64(view, to_idx(al)) == Some(1));
                if !step1 {
                    continue;
                }
                // Check that either `header` or `latch` branches on an `Icmp` comparing `p_inst`
                // or `back_v` with a loop-invariant bound.
                let check_br_exit = |blk: u32| -> bool {
                    let t = view.terminator(Block(blk));
                    if view.op(t) != Op::Br {
                        return false;
                    }
                    let cond = to_idx(at(view.extra, to_idx(at(view.data, t))));
                    if view.op(cond) != Op::Icmp {
                        return false;
                    }
                    let cat = to_idx(at(view.data, cond));
                    let Some(cc) = IntCond::from_word(at(view.extra, cat)) else {
                        return false;
                    };
                    if !matches!(
                        cc,
                        IntCond::Ult
                            | IntCond::Ule
                            | IntCond::Slt
                            | IntCond::Sle
                            | IntCond::Ne
                            | IntCond::Eq
                            | IntCond::Ugt
                            | IntCond::Uge
                            | IntCond::Sgt
                            | IntCond::Sge
                    ) {
                        return false;
                    }
                    let (cl, cr) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
                    let lhs_iv = cl == p_inst || cl == back_v;
                    let rhs_iv = cr == p_inst || cr == back_v;
                    (lhs_iv && !at(&in_loop, to_idx(at(inst_block, to_idx(cr)))))
                        || (rhs_iv && !at(&in_loop, to_idx(at(inst_block, to_idx(cl)))))
                };
                if check_br_exit(header) || check_br_exit(latch) {
                    found_iv = Some(p_inst);
                    break;
                }
            }
        }

        // Clear `in_loop` marks before next header.
        for &lb in &loop_blocks {
            set_at(&mut in_loop, to_idx(lb), false);
        }

        if let Some(iv_inst) = found_iv {
            loop_blocks.sort_unstable();
            loops.push(CountedLoop { iv_inst, blocks: loop_blocks });
        }
    }
    loops
}

fn analyze_aos_loop_detailed<A: Allocator + Copy>(
    view: &View<'_>,
    ptr_param: usize,
    struct_stride: u64,
    fields: &[StructField],
    alloc: &A,
) -> Option<DetailedSoaPlan<A>> {
    if struct_stride == 0 || fields.is_empty() || view.blocks() < 2 {
        return None;
    }
    for f in fields {
        if f.size == 0 || u64::from(f.offset).checked_add(u64::from(f.size))? > struct_stride {
            return None;
        }
    }

    let entry_params = view.params(Block(0));
    if ptr_param >= entry_params.len() {
        return None;
    }
    let ptr_idx = entry_params.start + ptr_param;
    let ptr_inst = to_u32(ptr_idx);
    if Ty(at(view.tys, ptr_idx)) != Ty::PTR {
        return None;
    }

    let len = view.len();
    let mut inst_block = filled(len, 0u32, alloc);
    for b in 0..view.blocks() {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
    }

    let loops = find_counted_loops(view, &inst_block, alloc);
    if loops.is_empty() {
        return None;
    }

    // Track every pointer value transitively derived from `ptr_inst` via `Op::PtrAdd`.
    let mut derived = filled(len, false, alloc);
    set_at(&mut derived, ptr_idx, true);
    for inst in 0..len {
        if view.op(inst) == Op::PtrAdd {
            let base = at(view.extra, to_idx(at(view.data, inst)));
            if at(&derived, to_idx(base)) {
                set_at(&mut derived, inst, true);
            }
        }
    }

    let mut is_hot = filled(fields.len(), false, alloc);
    let mut loads: Vec<AosLoadSite, A> = Vec::new_in(*alloc);

    // Audit every instruction in the function: every use of `ptr_inst` (or any `PtrAdd` derived
    // from it) MUST be either an intermediate `PtrAdd` or a matched non-volatile AoS field `Load`
    // inside one of our counted loops. Any escape, store, or unmatched access refuses the rewrite.
    for inst in 0..len {
        let mut uses_derived = false;
        let mut edge_uses_derived = false;
        each(view, inst, |item| match item {
            Item::Value(v) => {
                if at(&derived, to_idx(v)) {
                    uses_derived = true;
                }
            }
            Item::Edge(edge) => {
                if let Some((start, argc)) = edge.args {
                    for k in 0..argc {
                        if at(&derived, to_idx(at(view.extra, start + k))) {
                            edge_uses_derived = true;
                        }
                    }
                }
            }
        });
        if edge_uses_derived {
            return None;
        }
        if !uses_derived {
            continue;
        }

        let op = view.op(inst);
        if op == Op::PtrAdd {
            // Ensure the derived pointer is the base operand (not the integer offset).
            let idx = to_idx(at(view.data, inst));
            let (base, by) = (at(view.extra, idx), at(view.extra, idx + 1));
            if at(&derived, to_idx(base)) && !at(&derived, to_idx(by)) {
                continue;
            }
            return None;
        }

        if op != Op::Load || Flags(at(view.flags, inst)).contains(Flags::VOLATILE) {
            return None;
        }

        let idx = to_idx(at(view.data, inst));
        let load_ptr = at(view.extra, idx);
        let load_off = at(view.extra, idx + 1).cast_signed();
        let load_ty = Ty(at(view.tys, inst));
        let blk = at(&inst_block, inst);

        // Try matching against enclosing counted loops (innermost first: highest header block).
        let mut matched: Option<(u32, usize)> = None;
        for lp in loops.iter().rev() {
            if lp.blocks.binary_search(&blk).is_err() {
                continue;
            }
            let Some((scale, total_off)) =
                decompose_aos_ptr(view, load_ptr, load_off, ptr_inst, lp.iv_inst, *alloc)
            else {
                continue;
            };
            if scale != struct_stride || total_off < 0 {
                continue;
            }
            let Ok(off_u32) = u32::try_from(total_off) else {
                continue;
            };
            if let Some(fidx) = fields
                .iter()
                .position(|f| f.offset == off_u32 && f.ty == load_ty)
            {
                matched = Some((lp.iv_inst, fidx));
                break;
            }
        }

        let Some((iv_inst, field_idx)) = matched else {
            return None;
        };
        set_at(&mut is_hot, field_idx, true);
        loads.push(AosLoadSite { load_inst: inst, iv_inst, field_idx });
    }

    if loads.is_empty() {
        return None;
    }

    let mut hot_field_indices: Vec<usize, A> = Vec::new_in(*alloc);
    let mut cold_field_indices: Vec<usize, A> = Vec::new_in(*alloc);
    let mut hot_bytes = 0u64;
    for (idx, field) in fields.iter().enumerate() {
        if at(&is_hot, idx) {
            hot_field_indices.push(idx);
            hot_bytes = hot_bytes.saturating_add(u64::from(field.size));
        } else {
            cold_field_indices.push(idx);
        }
    }
    let hot_bytes = hot_bytes.min(struct_stride);
    let bandwidth_saved_pct =
        u32::try_from((struct_stride.saturating_sub(hot_bytes)) * 100 / struct_stride).unwrap_or(0);

    Some(DetailedSoaPlan {
        analysis: SoaAnalysis {
            struct_stride,
            hot_field_indices,
            cold_field_indices,
            hot_bytes,
            bandwidth_saved_pct,
        },
        ptr_inst,
        loads,
    })
}

/// Analyzes `view` for counted loops streaming over `ptr_param` with affine AoS addresses
/// `ptr_param + iv * struct_stride + field_off` (`Mul` or `Shl`, plus optional `PtrAdd` / `Load`
/// constant offsets) and classifies every field in `fields` into hot vs cold subsets.
pub fn analyze_aos_loop<A: Allocator + Copy>(
    view: View<'_>,
    ptr_param: usize,
    struct_stride: u64,
    fields: &[StructField],
    alloc: A,
) -> Option<SoaAnalysis<A>> {
    analyze_aos_loop_detailed(&view, ptr_param, struct_stride, fields, &alloc).map(|p| p.analysis)
}

fn push_inst<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    data: u32,
    attr: u32,
    loc: u32,
) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64, loc: u32) -> u32 {
    let bits = ty.bits(64);
    if bits < 32 {
        let masked = trunc_u32(val & ((1u64 << bits) - 1));
        push_inst(func, Op::Iconst, Flags::NONE, ty, masked, 0, loc)
    } else if bits == 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val), 0, loc)
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned(), 0, loc)
    } else {
        let idx = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, idx, 0, loc)
    }
}

fn push_bin<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    a: u32,
    b: u32,
    loc: u32,
) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, idx, 0, loc)
}

fn copy_inst_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst))),
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
                    Field::Target => {
                        let b = at(view.extra, p);
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(b);
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
                            let b = at(view.extra, p);
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(b);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask | Field::Blocks => {
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

/// Computes a safe `mem_attrs` index for a contiguous column load of element size `field_size`:
/// never promises stricter alignment than `field_size` (for example, when an offset-0 AoS load had
/// 64-byte struct alignment, the split `i64` column is aligned to at most 8 bytes).
fn column_mem_attr<A: Allocator>(
    view: &View<'_>,
    orig_attr: u32,
    field_size: u32,
    func: &mut Func<A>,
) -> u32 {
    let Some(row) = orig_attr.checked_sub(1) else {
        return 0;
    };
    let Some(&rec) = view.mem_attrs.get(to_idx(row)) else {
        return 0;
    };
    let max_align = u8::try_from(field_size.trailing_zeros().min(63)).unwrap_or(63);
    if rec.align <= max_align {
        return orig_attr;
    }
    let mut capped = rec;
    capped.align = max_align;
    if capped == MemAttrsRec::default() {
        return 0;
    }
    func.mem_attrs.push(capped);
    to_u32(func.mem_attrs.len())
}

/// Automatically rewrites an AoS streaming loop function into an SoA column-split function when
/// hot fields occupy strictly less than `struct_stride` bytes per element.
///
/// Replaces parameter `ptr_param` with `K = hot_field_indices.len()` contiguous column pointer
/// parameters (`col_0_ptr, ..., col_{K-1}_ptr`) and rewrites every hot field load into a
/// unit-stride contiguous column load (`col_k_ptr + (iv << log2(field_size))`), followed by
/// `dead` + `verify`.
pub fn rewrite_aos_to_soa<A: Allocator + Copy>(
    view: View<'_>,
    ptr_param: usize,
    struct_stride: u64,
    fields: &[StructField],
    alloc: A,
) -> Result<SoaOutput<A>, SimplifyError> {
    let Some(plan) = analyze_aos_loop_detailed(&view, ptr_param, struct_stride, fields, &alloc) else {
        return Ok(SoaOutput {
            func: view.to_func_in(alloc),
            stats: SoaStats::default(),
        });
    };

    let k_cols = plan.analysis.hot_field_indices.len();
    if k_cols == 0 || plan.analysis.hot_bytes >= struct_stride {
        return Ok(SoaOutput {
            func: view.to_func_in(alloc),
            stats: SoaStats::default(),
        });
    }

    // Map `field_idx -> column_index (0..k_cols)`.
    let mut field_to_col = filled(fields.len(), NONE, &alloc);
    for (col_idx, &fidx) in plan.analysis.hot_field_indices.iter().enumerate() {
        set_at(&mut field_to_col, fidx, to_u32(col_idx));
    }

    // Index matched load sites by instruction index for O(1) lookup during rebuild.
    let len = view.len();
    let mut load_site_idx = filled(len, NONE, &alloc);
    for (idx, site) in plan.loads.iter().enumerate() {
        set_at(&mut load_site_idx, site.load_inst, to_u32(idx));
    }

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

    // Append a new `SigRec` replacing `ptr_param` with `k_cols` contiguous `Ty::PTR` parameters.
    let own_sig = view.sig(SigId(view.head.sig));
    let new_first = u32::try_from(func.sig_params.len())
        .map_err(|_| SimplifyError::Refused { inst: 0, what: "sig_params overflow in SoA rewrite" })?;
    let new_param_count = own_sig
        .params
        .len()
        .checked_sub(1)
        .and_then(|n| n.checked_add(k_cols))
        .and_then(|n| u16::try_from(n).ok())
        .ok_or(SimplifyError::Refused { inst: 0, what: "parameter count overflow in SoA rewrite" })?;
    let col_param_rec = ParamRec {
        facts: 0,
        ty: Ty::PTR.0,
        ext: ext_byte(Ext::None),
        kind: param_kind_byte(ParamKind::Value),
    };
    for (p_idx, &prec) in own_sig.params.iter().enumerate() {
        if p_idx == ptr_param {
            for _ in 0..k_cols {
                func.sig_params.push(col_param_rec);
            }
        } else {
            func.sig_params.push(prec);
        }
    }
    for &rrec in own_sig.rets {
        func.sig_params.push(rrec);
    }
    let new_sig_idx = u32::try_from(func.sigs.len())
        .map_err(|_| SimplifyError::Refused { inst: 0, what: "sigs overflow in SoA rewrite" })?;
    func.sigs.push(SigRec {
        first: new_first,
        params: new_param_count,
        rets: u16::try_from(own_sig.rets.len()).unwrap_or(0),
        conv: own_sig.conv,
        bits: own_sig.bits,
        pad: [0; 2],
    });
    func.head.sig = new_sig_idx;

    let mut val_map = filled(len, NONE, &alloc);
    let mut col_ptrs: Vec<u32, A> = Vec::with_capacity_in(k_cols, alloc);

    for b in 0..view.blocks() {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(to_u32(b)));

        for inst in range {
            if b == 0 && inst == to_idx(plan.ptr_inst) {
                // Emit `K` contiguous column pointer parameters in place of `ptr_param`.
                let loc = at(view.locs, inst);
                for _ in 0..k_cols {
                    let cp = push_inst(&mut func, Op::Param, Flags::NONE, Ty::PTR, 0, 0, loc);
                    col_ptrs.push(cp);
                }
                // Bind old `ptr_inst` to `col_ptrs[0]` so dead AoS `PtrAdd`s stay well-typed
                // until `dead` eliminates them immediately below.
                set_at(&mut val_map, inst, col_ptrs.first().copied().unwrap_or(NONE));
                continue;
            }

            let site_id = at(&load_site_idx, inst);
            if site_id != NONE {
                let Some(&site) = plan.loads.get(to_idx(site_id)) else {
                    return Err(SimplifyError::Refused { inst, what: "invalid SoA load site" });
                };
                let Some(&field) = fields.get(site.field_idx) else {
                    return Err(SimplifyError::Refused { inst, what: "invalid SoA field index" });
                };
                let col_idx = to_idx(at(&field_to_col, site.field_idx));
                let col_base = at(&col_ptrs, col_idx);
                let loc = at(view.locs, inst);

                let iv_new = at(&val_map, to_idx(site.iv_inst));
                let iv_ty = Ty(at(view.tys, to_idx(site.iv_inst)));
                let iv64 = if iv_ty == Ty::I64 {
                    iv_new
                } else {
                    push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv_new, 0, loc)
                };

                let byte_off = if field.size == 1 {
                    iv64
                } else if field.size.is_power_of_two() {
                    let shift = u64::from(field.size.trailing_zeros());
                    let k_sh = push_iconst(&mut func, Ty::I64, shift, loc);
                    push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, iv64, k_sh, loc)
                } else {
                    let k_sz = push_iconst(&mut func, Ty::I64, u64::from(field.size), loc);
                    push_bin(&mut func, Op::Mul, Flags::NONE, Ty::I64, iv64, k_sz, loc)
                };

                let col_ptr = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, col_base, byte_off, loc);
                let mem_attr = column_mem_attr(&view, at(view.attrs, inst), field.size, &mut func);
                let ld_at = to_u32(func.extra.len());
                func.extra.push(col_ptr);
                func.extra.push(0); // contiguous column load has zero struct-field offset
                let new_load = push_inst(
                    &mut func,
                    Op::Load,
                    Flags(at(view.flags, inst)),
                    field.ty,
                    ld_at,
                    mem_attr,
                    loc,
                );
                set_at(&mut val_map, inst, new_load);
            } else {
                let mapped = copy_inst_remapped(&view, inst, &val_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    let cleaned = dead(func.view(), alloc)?;
    verify(cleaned.view()).map_err(SimplifyError::Verify)?;

    Ok(SoaOutput {
        func: cleaned,
        stats: SoaStats {
            loads_rewritten: to_u32(plan.loads.len()),
            columns_created: to_u32(k_cols),
            bandwidth_saved_pct: plan.analysis.bandwidth_saved_pct,
        },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Bodies, BodyId, Outcome, Val},
        func::{Conv, RefId, RefKind, SlotKind},
    };

    const CALLER_ID: BodyId = BodyId([0xA1; 32]);
    const CALLEE_ID: BodyId = BodyId([0xB2; 32]);
    const FUEL: u64 = 200_000;

    struct SingleCallee<'a> {
        callee: View<'a>,
    }

    impl Bodies for SingleCallee<'_> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            match (from, reference) {
                (CALLER_ID, RefId(0)) => Some((CALLEE_ID, self.callee)),
                _ => None,
            }
        }
    }

    fn fields_64b_8xi64() -> [StructField; 8] {
        [
            StructField { offset: 0, size: 8, ty: Ty::I64 },
            StructField { offset: 8, size: 8, ty: Ty::I64 },
            StructField { offset: 16, size: 8, ty: Ty::I64 },
            StructField { offset: 24, size: 8, ty: Ty::I64 },
            StructField { offset: 32, size: 8, ty: Ty::I64 },
            StructField { offset: 40, size: 8, ty: Ty::I64 },
            StructField { offset: 48, size: 8, ty: Ty::I64 },
            StructField { offset: 56, size: 8, ty: Ty::I64 },
        ]
    }

    /// Builds `fn sum_aos(ptr: PTR, n: I64) -> I64`:
    ///   for i in 0..n { acc += ptr[i].f0 + ptr[i].f4; }
    /// where each struct has `stride = 64` bytes (`8 x i64`).
    fn build_aos_64b_sum(use_shl: bool) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let byte_off = if use_shl {
            let k6 = b.iconst(Ty::I64, 6);
            b.binary(Op::Shl, Flags::NONE, i, k6)
        } else {
            let k64 = b.iconst(Ty::I64, 64);
            b.binary(Op::Mul, Flags::NONE, i, k64)
        };
        let elem_ptr = b.ptr_add(Flags::NONE, ptr, byte_off);
        let v0 = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(3));
        let v4 = if use_shl {
            // Exercise `PtrAdd(elem_ptr, 32)` + `Load(..., 0)` addressing form as well.
            let k32 = b.iconst(Ty::I64, 32);
            let f4_ptr = b.ptr_add(Flags::NONE, elem_ptr, k32);
            b.load(Ty::I64, Flags::NONE, f4_ptr, 0, MemAttrs::aligned(3))
        } else {
            b.load(Ty::I64, Flags::NONE, elem_ptr, 32, MemAttrs::aligned(3))
        };
        let pair = b.binary(Op::Add, Flags::NONE, v0, v4);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, pair);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        b.finish_checked().unwrap()
    }

    /// Harness allocating an AoS array of `<= 20` 64-byte structs, initializing `f0 = k*7 + 3`,
    /// `f4 = k*11 + 5`, and cold fields with distinct non-zero sentinels, then calling `callee(aos, n)`.
    fn build_aos_harness() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let h_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let k_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(h_sig);
        let k_ref = b.reference(RefKind::Func, 0);
        let s_aos = b.slot(64 * 20, 3, SlotKind::Named);
        let [entry, hdr, body, call_b] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let base = b.slot_addr(s_aos);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[zero]));

        b.begin(hdr);
        let k = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, cond, (body, &[]), (call_b, &[]));

        b.begin(body);
        let k6 = b.iconst(Ty::I64, 6);
        let off = b.binary(Op::Shl, Flags::NONE, k, k6);
        let ep = b.ptr_add(Flags::NONE, base, off);
        let c7 = b.iconst(Ty::I64, 7);
        let c3 = b.iconst(Ty::I64, 3);
        let c11 = b.iconst(Ty::I64, 11);
        let c5 = b.iconst(Ty::I64, 5);
        let cold_sentinel = b.iconst(Ty::I64, 0xDEAD);
        let t0 = b.binary(Op::Mul, Flags::NONE, k, c7);
        let f0 = b.binary(Op::Add, Flags::NONE, t0, c3);
        let t4 = b.binary(Op::Mul, Flags::NONE, k, c11);
        let f4 = b.binary(Op::Add, Flags::NONE, t4, c5);
        b.store(Flags::NONE, f0, ep, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, cold_sentinel, ep, 8, MemAttrs::aligned(3));
        b.store(Flags::NONE, cold_sentinel, ep, 16, MemAttrs::aligned(3));
        b.store(Flags::NONE, cold_sentinel, ep, 24, MemAttrs::aligned(3));
        b.store(Flags::NONE, f4, ep, 32, MemAttrs::aligned(3));
        b.store(Flags::NONE, cold_sentinel, ep, 40, MemAttrs::aligned(3));
        b.store(Flags::NONE, cold_sentinel, ep, 48, MemAttrs::aligned(3));
        b.store(Flags::NONE, cold_sentinel, ep, 56, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((hdr, &[k2]));

        b.begin(call_b);
        let res = b.call(Flags::NONE, k_ref, k_sig, &[base, n]);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    /// Harness allocating two contiguous `i64` column arrays (`col0`, `col1`) initialized with
    /// identical `f0 = k*7 + 3`, `f4 = k*11 + 5` values, then calling `callee(col0, col1, n)`.
    fn build_soa_harness() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let h_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let k_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(h_sig);
        let k_ref = b.reference(RefKind::Func, 0);
        let s_col0 = b.slot(8 * 20, 3, SlotKind::Named);
        let s_col1 = b.slot(8 * 20, 3, SlotKind::Named);
        let [entry, hdr, body, call_b] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let p0 = b.slot_addr(s_col0);
        let p1 = b.slot_addr(s_col1);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[zero]));

        b.begin(hdr);
        let k = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, cond, (body, &[]), (call_b, &[]));

        b.begin(body);
        let k3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, k, k3);
        let ep0 = b.ptr_add(Flags::NONE, p0, off);
        let ep1 = b.ptr_add(Flags::NONE, p1, off);
        let c7 = b.iconst(Ty::I64, 7);
        let c3 = b.iconst(Ty::I64, 3);
        let c11 = b.iconst(Ty::I64, 11);
        let c5 = b.iconst(Ty::I64, 5);
        let t0 = b.binary(Op::Mul, Flags::NONE, k, c7);
        let f0 = b.binary(Op::Add, Flags::NONE, t0, c3);
        let t4 = b.binary(Op::Mul, Flags::NONE, k, c11);
        let f4 = b.binary(Op::Add, Flags::NONE, t4, c5);
        b.store(Flags::NONE, f0, ep0, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, f4, ep1, 0, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((hdr, &[k2]));

        b.begin(call_b);
        let res = b.call(Flags::NONE, k_ref, k_sig, &[p0, p1, n]);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn aos_to_soa_splits_64b_struct_and_unlocks_4x_simd() {
        let fields = fields_64b_8xi64();
        let aos_fn = build_aos_64b_sum(false);

        // Before SoA splitting, the 64-byte strided AoS loop cannot be unit-stride SIMD vectorized.
        let before_vec = crate::vectorize::auto_vectorize_loops(aos_fn.view(), TestHeap).unwrap();
        assert_eq!(before_vec.stats.loops_vectorized, 0);

        // Analyze hot vs cold fields: only f0 (8B) and f4 (8B) are hot -> 16B / 64B = 75% saved.
        let analysis = analyze_aos_loop(aos_fn.view(), 0, 64, &fields, TestHeap)
            .expect("must recognize 64B AoS loop");
        assert_eq!(analysis.struct_stride, 64);
        assert_eq!(analysis.hot_field_indices.as_slice(), &[0, 4]);
        assert_eq!(analysis.cold_field_indices.as_slice(), &[1, 2, 3, 5, 6, 7]);
        assert_eq!(analysis.hot_bytes, 16);
        assert_eq!(analysis.bandwidth_saved_pct, 75);

        // Rewrite AoS -> SoA (2 contiguous column pointer parameters replacing `ptr`).
        let soa = rewrite_aos_to_soa(aos_fn.view(), 0, 64, &fields, TestHeap).unwrap();
        assert_eq!(soa.stats.bandwidth_saved_pct, 75);
        assert_eq!(soa.stats.columns_created, 2);
        assert_eq!(soa.stats.loads_rewritten, 2);
        assert_eq!(soa.func.view().params(Block(0)).len(), 3);

        // The rewritten SoA loop now streams at unit stride and IMMEDIATELY vectorizes 4x SIMD!
        let after_vec = crate::vectorize::auto_vectorize_loops(soa.func.view(), TestHeap).unwrap();
        assert_eq!(
            after_vec.stats.loops_vectorized, 1,
            "rewritten SoA loop must unlock 4x-unrolled SIMD vectorization"
        );
        let vv = after_vec.func.view();
        assert!(
            (0..vv.len()).any(|i| vv.op(i) == Op::Load && Ty(vv.tys[i]).is_vector()),
            "vectorized SoA loop must emit SIMD vector loads"
        );
        assert!(
            (0..vv.len()).any(|i| vv.op(i) == Op::ReduceAdd),
            "vectorized SoA loop must emit horizontal ReduceAdd"
        );

        // Prove bit-exact equivalence between AoS and SoA via `eval::run_calls_in`.
        let aos_harness = build_aos_harness();
        let soa_harness = build_soa_harness();
        for &n in &[0u128, 1, 2, 3, 7, 8, 9, 15, 16, 17, 20] {
            let Outcome::Returned(r_aos) = eval::run_calls_in(
                aos_harness.view(),
                CALLER_ID,
                &[Val::Int(n)],
                FUEL,
                &SingleCallee { callee: aos_fn.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("AoS eval failed for n={n}");
            };
            let Outcome::Returned(r_soa) = eval::run_calls_in(
                soa_harness.view(),
                CALLER_ID,
                &[Val::Int(n)],
                FUEL,
                &SingleCallee { callee: soa.func.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("SoA eval failed for n={n}");
            };
            let expected: u128 = (0..n).map(|k| (k * 7 + 3) + (k * 11 + 5)).sum();
            assert_eq!(r_aos.as_slice(), &[Val::Int(expected)], "AoS sum mismatch at n={n}");
            assert_eq!(r_soa, r_aos, "SoA vs AoS mismatch at n={n}");
        }
    }

    #[test]
    fn aos_to_soa_handles_shl_stride_and_chained_ptr_add() {
        let fields = fields_64b_8xi64();
        let aos_shl = build_aos_64b_sum(true);

        let soa = rewrite_aos_to_soa(aos_shl.view(), 0, 64, &fields, TestHeap).unwrap();
        assert_eq!(soa.stats.bandwidth_saved_pct, 75);
        assert_eq!(soa.stats.columns_created, 2);
        assert_eq!(soa.stats.loads_rewritten, 2);

        let after_vec = crate::vectorize::auto_vectorize_loops(soa.func.view(), TestHeap).unwrap();
        assert_eq!(after_vec.stats.loops_vectorized, 1);

        let aos_harness = build_aos_harness();
        let soa_harness = build_soa_harness();
        for &n in &[0u128, 1, 5, 12, 20] {
            let Outcome::Returned(r_aos) = eval::run_calls_in(
                aos_harness.view(),
                CALLER_ID,
                &[Val::Int(n)],
                FUEL,
                &SingleCallee { callee: aos_shl.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("AoS shl eval failed");
            };
            let Outcome::Returned(r_soa) = eval::run_calls_in(
                soa_harness.view(),
                CALLER_ID,
                &[Val::Int(n)],
                FUEL,
                &SingleCallee { callee: soa.func.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("SoA shl eval failed");
            };
            assert_eq!(r_soa, r_aos, "Shl+PtrAdd SoA mismatch at n={n}");
        }
    }
}

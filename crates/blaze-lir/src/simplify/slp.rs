//! Straight-Line SLP Vectorization (`slp_vectorize`) & SIMD `memchr` Byte-Scan Idiom
//! (`vectorize_memchr_loops`).
//!
//! 1. **SLP (Superword-Level Parallelism) Vectorization**: finds contiguous 128-bit scalar store
//!    groups (`4 x i32/f32` or `2 x i64/f64`) inside any straight-line basic block whose stored
//!    values form an isomorphic pure ALU/load/splat tree, and packs the entire tree into single
//!    128-bit SIMD loads, vector ALU ops (enabling NEON `eor3` / `fmla` and AVX2 3-op ops), and a
//!    single 128-bit SIMD store — no counted loop required!
//! 2. **SIMD `memchr` Byte-Scan Idiom**: detects scalar byte-search loops
//!    (`while i < n { if ptr[i] == needle { return/break i; } i += 1; }`) and prepends a
//!    16-byte-per-iteration SIMD scan (`load v16xi8 -> cmeq -> select -> reduce_or != 0`) that
//!    hands off to the exact scalar loop on the matching 16-byte chunk or short tail.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, dead, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Scalar, Ty, View,
    arith::{iconst_value, wide_value},
    op::{Field, Layout},
    verify,
};

/// Telemetry returned by [`slp_vectorize`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SlpStats {
    pub trees_vectorized: u32,
}

/// Output of [`slp_vectorize`].
#[derive(Debug)]
pub struct SlpOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SlpStats,
}

/// Telemetry returned by [`vectorize_memchr_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MemchrStats {
    pub loops_vectorized: u32,
}

/// Output of [`vectorize_memchr_loops`].
#[derive(Debug)]
pub struct MemchrOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: MemchrStats,
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

/// Decomposes a pointer expression into `(base_ptr, byte_offset)` across one `PtrAdd(base, const)`.
#[inline]
fn decompose_ptr_off(view: &View<'_>, ptr: u32, extra_off: i32) -> (u32, i64) {
    let pi = to_idx(ptr);
    if view.op(pi) == Op::PtrAdd {
        let idx = to_idx(at(view.data, pi));
        let (base, by) = (at(view.extra, idx), at(view.extra, idx + 1));
        if let Some(c) = const_u64(view, to_idx(by)) {
            return (base, c.cast_signed().wrapping_add(i64::from(extra_off)));
        }
    }
    (ptr, i64::from(extra_off))
}

#[inline]
fn slp_elem_info(ty: Ty) -> Option<(Ty, usize, i64)> {
    if ty == Ty::I32 {
        Some((Ty::vector(Scalar::I32, 4)?, 4, 4))
    } else if ty == Ty::F32 {
        Some((Ty::vector(Scalar::F32, 4)?, 4, 4))
    } else if ty == Ty::I64 {
        Some((Ty::vector(Scalar::I64, 2)?, 2, 8))
    } else if ty == Ty::F64 {
        Some((Ty::vector(Scalar::F64, 2)?, 2, 8))
    } else {
        None
    }
}

#[inline]
fn is_slp_alu_op(op: Op, elem_ty: Ty) -> bool {
    if elem_ty.is_float() {
        matches!(op, Op::Fadd | Op::Fsub | Op::Fmul)
    } else {
        matches!(op, Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor)
    }
}

#[derive(Clone, Copy)]
enum SlpNode {
    /// Contiguous 128-bit vector load from `(base_ptr, byte_off, mem_attr)`.
    Load { base: u32, off: i32, mem_attr: u32 },
    /// Broadcast a single scalar value across all lanes.
    Splat(u32),
    /// Binary vector ALU op over two child `SlpNode` indices in `nodes`.
    Bin { op: Op, flags: Flags, lhs: usize, rhs: usize },
}

enum SlpFrame {
    Visit { vals: [u32; 4], depth: u32 },
    EmitBin { op: Op, flags: Flags },
}

fn build_slp_tree<A: Allocator + Copy>(
    view: &View<'_>,
    block_start: usize,
    first_store: usize,
    root_vals: [u32; 4],
    lanes: usize,
    elem_ty: Ty,
    elem_bytes: i64,
    nodes: &mut Vec<SlpNode, A>,
    alloc: A,
) -> Option<usize> {
    let mut frames: Vec<SlpFrame, A> = Vec::with_capacity_in(32, alloc);
    let mut results: Vec<usize, A> = Vec::with_capacity_in(32, alloc);
    frames.push(SlpFrame::Visit { vals: root_vals, depth: 0 });

    while let Some(frame) = frames.pop() {
        match frame {
            SlpFrame::EmitBin { op, flags } => {
                let rhs = results.pop()?;
                let lhs = results.pop()?;
                let idx = nodes.len();
                nodes.push(SlpNode::Bin { op, flags, lhs, rhs });
                results.push(idx);
            }
            SlpFrame::Visit { vals, depth } => {
                if depth > 8 || nodes.len() > 32 {
                    return None;
                }
                let lanes_vals = vals.get(..lanes)?;
                let v0 = vals[0];
                // 1. All lanes identical -> `Splat`!
                if lanes_vals.iter().all(|&v| v == v0) {
                    let idx = nodes.len();
                    nodes.push(SlpNode::Splat(v0));
                    results.push(idx);
                    continue;
                }
                let v0_idx = to_idx(v0);
                let op0 = view.op(v0_idx);
                let fl0 = Flags(at(view.flags, v0_idx));
                if !lanes_vals.iter().all(|&v| {
                    let vi = to_idx(v);
                    view.op(vi) == op0 && Flags(at(view.flags, vi)) == fl0 && Ty(at(view.tys, vi)) == elem_ty
                }) {
                    return None;
                }

                // 2. Contiguous 128-bit `Op::Load` group (all inside this block before `first_store`, non-volatile).
                if op0 == Op::Load && !fl0.contains(Flags::VOLATILE) {
                    if !lanes_vals.iter().all(|&v| {
                        let vi = to_idx(v);
                        vi >= block_start && vi < first_store
                    }) {
                        return None;
                    }
                    let at0 = to_idx(at(view.data, v0_idx));
                    let (base0, off0) =
                        decompose_ptr_off(view, at(view.extra, at0), at(view.extra, at0 + 1).cast_signed());
                    let off0_i32 = i32::try_from(off0).ok()?;
                    for (k, &vk) in lanes_vals.iter().enumerate().skip(1) {
                        let atk = to_idx(at(view.data, to_idx(vk)));
                        let (bk, ok) =
                            decompose_ptr_off(view, at(view.extra, atk), at(view.extra, atk + 1).cast_signed());
                        let k_i64 = i64::try_from(k).ok()?;
                        if bk != base0 || ok != off0 + k_i64 * elem_bytes {
                            return None;
                        }
                    }
                    let idx = nodes.len();
                    nodes.push(SlpNode::Load {
                        base: base0,
                        off: off0_i32,
                        mem_attr: at(view.attrs, v0_idx),
                    });
                    results.push(idx);
                    continue;
                }

                // 3. Isomorphic binary vector ALU op!
                if is_slp_alu_op(op0, elem_ty) && op0.info().flags.meanings_only(fl0).0 == 0 {
                    let mut lhs_v = [0u32; 4];
                    let mut rhs_v = [0u32; 4];
                    for (k, &vk) in lanes_vals.iter().enumerate() {
                        let idx = to_idx(at(view.data, to_idx(vk)));
                        set_at(&mut lhs_v, k, at(view.extra, idx));
                        set_at(&mut rhs_v, k, at(view.extra, idx + 1));
                    }
                    frames.push(SlpFrame::EmitBin { op: op0, flags: fl0 });
                    frames.push(SlpFrame::Visit { vals: rhs_v, depth: depth + 1 });
                    frames.push(SlpFrame::Visit { vals: lhs_v, depth: depth + 1 });
                    continue;
                }

                return None;
            }
        }
    }

    results.pop()
}

fn push_inst<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, data: u32, attr: u32) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(0);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64) -> u32 {
    if ty.bits(64) <= 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val), 0)
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned(), 0)
    } else {
        let idx = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, idx, 0)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, idx, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, ty, idx, 0)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) {
    let idx = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, idx, 0);
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
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, idx, 0);
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: Option<&[u32]>,
    func: &mut Func<A>,
) -> u32 {
    let map_b = |b: u32| blk_map.map_or(b, |m| at(m, to_idx(b)));
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst))),
        Layout::Data(Field::Block) => map_b(at(view.data, inst)),
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
                        func.extra.push(map_b(at(view.extra, p)));
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
                            func.extra.push(map_b(at(view.extra, p + k)));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(map_b(at(view.extra, p)));
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
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
                            func.extra.push(map_b(at(view.extra, p)));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
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

/// Packs contiguous straight-line 128-bit scalar store trees (`4x i32/f32` or `2x i64/f64`)
/// into native 128-bit SIMD loads, vector ALU ops, and vector stores.
pub fn slp_vectorize<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<SlpOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if len < 4 || view.head.ptr_bits != 64 {
        return Ok(SlpOutput { func: view.to_func_in(alloc), stats: SlpStats::default() });
    }

    struct SlpPlan<A: Allocator> {
        store_insts: [usize; 4],
        lanes: usize,
        vec_ty: Ty,
        dst_base: u32,
        dst_off: i32,
        dst_attr: u32,
        nodes: Vec<SlpNode, A>,
        root: usize,
    }

    let mut plans: Vec<SlpPlan<A>, A> = Vec::new_in(alloc);
    let mut skip_store = filled(len, false, &alloc);

    for b in 0..blocks {
        let range = view.block(Block(to_u32(b)));
        // Collect consecutive non-volatile scalar stores within pure segments (no intervening
        // Call/Atomic/Fence/other Store that could alias).
        let mut st_list: Vec<usize, A> = Vec::new_in(alloc);
        let mut seg_start = range.start;
        for inst in range.start..(range.end - 1) {
            let op = view.op(inst);
            if op == Op::Store && !Flags(at(view.flags, inst)).contains(Flags::VOLATILE) {
                st_list.push(inst);
                continue;
            }
            if matches!(
                op,
                Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::AtomicLoad
                    | Op::AtomicStore
                    | Op::AtomicRmw
                    | Op::Cmpxchg
                    | Op::Fence
                    | Op::Memcpy
                    | Op::Memmove
                    | Op::Memset
            ) {
                st_list.clear();
                seg_start = inst + 1;
            }
        }
        if st_list.len() < 2 {
            continue;
        }

        // Try matching contiguous groups of `lanes` stores in order!
        let mut i = 0;
        while i < st_list.len() {
            let s0 = at(&st_list, i);
            if at(&skip_store, s0) {
                i += 1;
                continue;
            }
            let at0 = to_idx(at(view.data, s0));
            let v0 = at(view.extra, at0);
            let elem_ty = Ty(at(view.tys, to_idx(v0)));
            let Some((vec_ty, lanes, elem_bytes)) = slp_elem_info(elem_ty) else {
                i += 1;
                continue;
            };
            if i + lanes > st_list.len() {
                i += 1;
                continue;
            }
            let (base0, off0) = decompose_ptr_off(&view, at(view.extra, at0 + 1), at(view.extra, at0 + 2).cast_signed());
            let Ok(off0_i32) = i32::try_from(off0) else {
                i += 1;
                continue;
            };
            let mut group = [0usize; 4];
            let mut vals = [0u32; 4];
            group[0] = s0;
            vals[0] = v0;
            let mut contiguous = true;
            for k in 1..lanes {
                let sk = at(&st_list, i + k);
                let atk = to_idx(at(view.data, sk));
                let vk = at(view.extra, atk);
                let (bk, ok) =
                    decompose_ptr_off(&view, at(view.extra, atk + 1), at(view.extra, atk + 2).cast_signed());
                let k_i64 = i64::try_from(k).unwrap_or(0);
                if Ty(at(view.tys, to_idx(vk))) != elem_ty || bk != base0 || ok != off0 + k_i64 * elem_bytes {
                    contiguous = false;
                    break;
                }
                set_at(&mut group, k, sk);
                set_at(&mut vals, k, vk);
            }
            if !contiguous {
                i += 1;
                continue;
            }
            // Verify no other store lies between `seg_start` and `group[0]` that could interfere with loads,
            // by using `first_store = if i == 0 { s0 } else { st_list[i - 1] + 1 }` as the safe load window!
            let safe_load_start = if i == 0 { seg_start } else { at(&st_list, i - 1) + 1 };
            let mut nodes = Vec::new_in(alloc);
            if let Some(root) = build_slp_tree(&view, safe_load_start, s0, vals, lanes, elem_ty, elem_bytes, &mut nodes, alloc) {
                // Only vectorize if the tree actually contains a vector Load or ALU op (not just trivial splat store)!
                let has_work = nodes.iter().any(|n| !matches!(n, SlpNode::Splat(_)));
                if has_work {
                    for &st in group.get(..lanes).unwrap_or(&[]) {
                        set_at(&mut skip_store, st, true);
                    }
                    plans.push(SlpPlan {
                        store_insts: group,
                        lanes,
                        vec_ty,
                        dst_base: base0,
                        dst_off: off0_i32,
                        dst_attr: at(view.attrs, s0),
                        nodes,
                        root,
                    });
                    i += lanes;
                    continue;
                }
            }
            i += 1;
        }
    }

    if plans.is_empty() {
        return Ok(SlpOutput { func: view.to_func_in(alloc), stats: SlpStats::default() });
    }

    let trees_vectorized = to_u32(plans.len());
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
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        for inst in view.block(Block(to_u32(b))) {
            if let Some(plan) = plans.iter().find(|p| p.store_insts.get(p.lanes.saturating_sub(1)).copied() == Some(inst)) {
                // Emit packed 128-bit SIMD tree + single 128-bit vector store!
                let mut emitted_nodes: Vec<u32, A> = Vec::with_capacity_in(plan.nodes.len(), alloc);
                for &node in &plan.nodes {
                    let v = match node {
                        SlpNode::Load { base, off, mem_attr } => {
                            let idx = to_u32(func.extra.len());
                            func.extra.push(at(&val_map, to_idx(base)));
                            func.extra.push(off.cast_unsigned());
                            push_inst(&mut func, Op::Load, Flags::NONE, plan.vec_ty, idx, mem_attr)
                        }
                        SlpNode::Splat(s) => {
                            push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, at(&val_map, to_idx(s)), 0)
                        }
                        SlpNode::Bin { op, flags, lhs, rhs } => {
                            push_bin(&mut func, op, flags, plan.vec_ty, at(&emitted_nodes, lhs), at(&emitted_nodes, rhs))
                        }
                    };
                    emitted_nodes.push(v);
                }
                let idx = to_u32(func.extra.len());
                func.extra.push(at(&emitted_nodes, plan.root));
                func.extra.push(at(&val_map, to_idx(plan.dst_base)));
                func.extra.push(plan.dst_off.cast_unsigned());
                set_at(&mut val_map, inst, push_inst(&mut func, Op::Store, Flags::NONE, Ty::NONE, idx, plan.dst_attr));
                continue;
            }
            if at(&skip_store, inst) {
                continue;
            }
            let mapped = copy_remapped(&view, inst, &val_map, None, &mut func);
            set_at(&mut val_map, inst, mapped);
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;
    Ok(SlpOutput {
        func: cleaned,
        stats: SlpStats { trees_vectorized },
    })
}

/// Recognizes a scalar `memchr` byte-search loop (`while i < n { if ptr[i] == needle { found(i); } i += 1; }`)
/// and prepends a 16-byte-per-iteration SIMD vector scan (`load v16xi8 -> cmeq -> select -> reduce_or != 0`),
/// handing off directly to the exact scalar loop on the first matching 16-byte chunk or short tail!
pub fn vectorize_memchr_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<MemchrOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 4 || view.head.ptr_bits != 64 {
        return Ok(MemchrOutput { func: view.to_func_in(alloc), stats: MemchrStats::default() });
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

    struct MemchrPlan {
        preheader: u32,
        header: u32,
        iv_init: u32,
        bound: u32,
        base_ptr: u32,
        needle: u32,
        mem_attr: u32,
    }

    let mut found_plan: Option<MemchrPlan> = None;
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, latch) = if p0 < header && p1 > header {
            (p0, p1)
        } else if p1 < header && p0 > header {
            (p1, p0)
        } else {
            continue;
        };
        let pre_term = view.terminator(Block(preheader));
        if view.op(pre_term) != Op::Jump {
            continue;
        }
        let pre_at = to_idx(at(view.data, pre_term));
        let h_params = view.params(Block(header));
        if at(view.extra, pre_at) != header || h_params.len() != 1 || at(view.extra, pre_at + 1) != 1 {
            continue;
        }
        let iv_inst = to_u32(h_params.start);
        if Ty(at(view.tys, h_params.start)) != Ty::I64 {
            continue;
        }
        let iv_init = at(view.extra, pre_at + 2);

        // Header checks `i < n` and branches to `check` vs `not_found`.
        let h_term = view.terminator(Block(header));
        if view.op(h_term) != Op::Br {
            continue;
        }
        let hat = to_idx(at(view.data, h_term));
        let h_cv = to_idx(at(view.extra, hat));
        let check_blk = at(view.extra, hat + 1);
        let t_argc = to_idx(at(view.extra, hat + 2));
        if t_argc != 0 || view.op(h_cv) != Op::Icmp || preds.get(to_idx(check_blk)).map(Vec::len) != Some(1) {
            continue;
        }
        let hcat = to_idx(at(view.data, h_cv));
        if IntCond::from_word(at(view.extra, hcat)) != Some(IntCond::Ult) || at(view.extra, hcat + 1) != iv_inst {
            continue;
        }
        let bound = at(view.extra, hcat + 2);
        let bound_blk = at(&inst_block, to_idx(bound));
        if bound_blk == header || bound_blk == check_blk || bound_blk == latch {
            continue;
        }

        // `check_blk` loads `u8` at `PtrAdd(base_ptr, i)`, compares `== needle` (or `!=`), and branches to `latch` on miss!
        let crange = view.block(Block(check_blk));
        let cterm = crange.end - 1;
        if view.op(cterm) != Op::Br || !view.params(Block(check_blk)).is_empty() {
            continue;
        }
        let cat = to_idx(at(view.data, cterm));
        let c_cv = to_idx(at(view.extra, cat));
        let ct_blk = at(view.extra, cat + 1);
        let ct_argc = to_idx(at(view.extra, cat + 2));
        let ce_blk = at(view.extra, cat + 3 + ct_argc);
        if view.op(c_cv) != Op::Icmp {
            continue;
        }
        let ccat = to_idx(at(view.data, c_cv));
        let Some(ccond) = IntCond::from_word(at(view.extra, ccat)) else { continue };
        let step_matches = (ccond == IntCond::Eq && ce_blk == latch && ct_blk != latch)
            || (ccond == IntCond::Ne && ct_blk == latch && ce_blk != latch);
        if !step_matches {
            continue;
        }
        let (ca_u32, cb_u32) = (at(view.extra, ccat + 1), at(view.extra, ccat + 2));
        let (ca, cb) = (to_idx(ca_u32), to_idx(cb_u32));
        let (load_i, needle) = if view.op(ca) == Op::Load && at(&inst_block, ca) == check_blk {
            (ca, cb_u32)
        } else if view.op(cb) == Op::Load && at(&inst_block, cb) == check_blk {
            (cb, ca_u32)
        } else {
            continue;
        };
        if Ty(at(view.tys, load_i)) != Ty::I8 || Flags(at(view.flags, load_i)).contains(Flags::VOLATILE) {
            continue;
        }
        let needle_blk = at(&inst_block, to_idx(needle));
        if needle_blk == header || needle_blk == check_blk || needle_blk == latch {
            continue;
        }
        let lat = to_idx(at(view.data, load_i));
        let ptr_v = to_idx(at(view.extra, lat));
        let off = at(view.extra, lat + 1).cast_signed();
        if off != 0 || view.op(ptr_v) != Op::PtrAdd {
            continue;
        }
        let pat = to_idx(at(view.data, ptr_v));
        let (base_ptr, idx_v) = (at(view.extra, pat), at(view.extra, pat + 1));
        let base_blk = at(&inst_block, to_idx(base_ptr));
        if idx_v != iv_inst || base_blk == header || base_blk == check_blk || base_blk == latch {
            continue;
        }

        // `latch` increments `i2 = i + 1` and jumps back to `header(i2)` (no side effects!).
        let lrange = view.block(Block(latch));
        let lterm = lrange.end - 1;
        if preds.get(to_idx(latch)).map(Vec::len) != Some(1)
            || !view.params(Block(latch)).is_empty()
            || view.op(lterm) != Op::Jump
        {
            continue;
        }
        let ljat = to_idx(at(view.data, lterm));
        if at(view.extra, ljat) != header || at(view.extra, ljat + 1) != 1 {
            continue;
        }
        let iv_next = to_idx(at(view.extra, ljat + 2));
        if view.op(iv_next) != Op::Add {
            continue;
        }
        let aat = to_idx(at(view.data, iv_next));
        let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
        let step1 = (al == iv_inst && const_u64(&view, to_idx(ar)) == Some(1))
            || (ar == iv_inst && const_u64(&view, to_idx(al)) == Some(1));
        if !step1 {
            continue;
        }
        // Check `check_blk` and `latch` have no side-effecting instructions other than `load_i`.
        let pure_check = (crange.start..cterm).all(|k| k == load_i || matches!(view.op(k), Op::PtrAdd | Op::Icmp | Op::Iconst));
        let pure_latch = (lrange.start..lterm).all(|k| matches!(view.op(k), Op::Add | Op::Iconst));
        if !pure_check || !pure_latch {
            continue;
        }

        found_plan = Some(MemchrPlan {
            preheader,
            header,
            iv_init,
            bound,
            base_ptr,
            needle,
            mem_attr: at(view.attrs, load_i),
        });
        break;
    }

    let Some(plan) = found_plan else {
        return Ok(MemchrOutput { func: view.to_func_in(alloc), stats: MemchrStats::default() });
    };

    let (Some(vec_ty), Some(mask_ty)) = (Ty::vector(Scalar::I8, 16), Ty::vector(Scalar::I1, 16)) else {
        return Ok(MemchrOutput { func: view.to_func_in(alloc), stats: MemchrStats::default() });
    };

    // Splice 3 blocks (`vpre`, `vloop`, `vstep`) right after `plan.preheader`!
    let insert_at = to_idx(plan.preheader) + 1;
    let vpre_blk = to_u32(insert_at);
    let vloop_blk = to_u32(insert_at + 1);
    let vstep_blk = to_u32(insert_at + 2);
    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + 3) });
    }
    let orig_hdr_new = at(&blk_map, to_idx(plan.header));

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
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        if b_u32 == plan.preheader {
            let term = range.end - 1;
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, Some(&blk_map), &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let iv_init = at(&val_map, to_idx(plan.iv_init));
            let bound = at(&val_map, to_idx(plan.bound));
            let base_ptr = at(&val_map, to_idx(plan.base_ptr));
            let needle = at(&val_map, to_idx(plan.needle));

            let has_rem = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_init, bound);
            let rem = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, bound, iv_init);
            let k64 = push_iconst(&mut func, Ty::I64, 64);
            let enough = push_icmp(&mut func, IntCond::Uge, Ty::I1, rem, k64);
            let go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);
            push_br(&mut func, go_vec, vpre_blk, &[], orig_hdr_new, &[iv_init]);

            // --- `vpre`: hoist splats & compute `vec_end = iv_init + (rem & !63)` ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let kmask = push_iconst(&mut func, Ty::I64, !63u64);
            let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, kmask);
            let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init, vcnt);
            let vneedle = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, needle, 0);
            let z8 = push_iconst(&mut func, Ty::I8, 0);
            let ff8 = push_iconst(&mut func, Ty::I8, 0xff);
            let vzero = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, z8, 0);
            let vff = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, ff8, 0);
            push_jump(&mut func, vloop_blk, &[iv_init]);

            // --- `vloop`: scan 64 bytes/iter (4x16B `ldp q, q` x2 + `4x cmeq` + `3x orr` + 1-op `umaxv.16b`!) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
            let p = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, base_ptr, iv);
            let mut hits = [0u32; 4];
            for u in 0..4usize {
                let lat = to_u32(func.extra.len());
                func.extra.push(p);
                func.extra.push(to_u32(u * 16));
                let chunk = push_inst(&mut func, Op::Load, Flags::NONE, vec_ty, lat, plan.mem_attr);
                let vmask = push_icmp(&mut func, IntCond::Eq, mask_ty, chunk, vneedle);
                let sat = to_u32(func.extra.len());
                func.extra.push(vmask);
                func.extra.push(vff);
                func.extra.push(vzero);
                set_at(&mut hits, u, push_inst(&mut func, Op::Select, Flags::NONE, vec_ty, sat, 0));
            }
            let h01 = push_bin(&mut func, Op::Or, Flags::NONE, vec_ty, hits[0], hits[1]);
            let h23 = push_bin(&mut func, Op::Or, Flags::NONE, vec_ty, hits[2], hits[3]);
            let hall = push_bin(&mut func, Op::Or, Flags::NONE, vec_ty, h01, h23);
            let any = push_inst(&mut func, Op::ReduceUmax, Flags::NONE, Ty::I8, hall, 0);
            let matched = push_icmp(&mut func, IntCond::Ne, Ty::I1, any, z8);
            push_br(&mut func, matched, orig_hdr_new, &[iv], vstep_blk, &[]);

            // --- `vstep`: advance by 64 bytes ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv, k64);
            let keep = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_next, vend);
            push_br(&mut func, keep, vloop_blk, &[iv_next], orig_hdr_new, &[iv_next]);
        } else {
            for inst in range {
                let mapped = copy_remapped(&view, inst, &val_map, Some(&blk_map), &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(MemchrOutput {
        func,
        stats: MemchrStats { loops_vectorized: 1 },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::Conv,
    };

    #[test]
    fn slp_packs_straight_line_4x_f32_vector_add_into_single_128b_simd() {
        // Straight-line `[f32; 4]` add: `for k in 0..4 { dst[k] = a[k] + b[k]; }` (unrolled, no loop!).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        b.set_signature(sig);
        let e = b.create_block(BlockKind::Normal);
        b.begin(e);
        let dst = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let mut vals = [crate::Value(0); 4];
        for k in 0..4usize {
            let off = (k * 4) as i32;
            let va = b.load(Ty::F32, Flags::NONE, pa, off, MemAttrs::aligned(4));
            let vb = b.load(Ty::F32, Flags::NONE, pb, off, MemAttrs::aligned(4));
            vals[k] = b.binary(Op::Fadd, Flags::NONE, va, vb);
        }
        for k in 0..4usize {
            b.store(Flags::NONE, vals[k], dst, (k * 4) as i32, MemAttrs::aligned(4));
        }
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let out = slp_vectorize(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.trees_vectorized, 1);
        let vloads = (0..out.func.view().len())
            .filter(|&i| out.func.view().op(i) == Op::Load && Ty(out.func.view().tys[i]).is_vector())
            .count();
        assert_eq!(vloads, 2, "expected 2 packed 128-bit vector loads!");
        let vadds = (0..out.func.view().len())
            .filter(|&i| out.func.view().op(i) == Op::Fadd && Ty(out.func.view().tys[i]).is_vector())
            .count();
        assert_eq!(vadds, 1, "expected 1 packed 128-bit vector fadd!");
    }

    #[test]
    fn memchr_idiom_vectorizes_scalar_byte_search_loop() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I8)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, check, step, found, not_found] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let needle = b.param(Ty::I8);
        let z = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[z]));
        b.begin(hdr);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (check, &[]), (not_found, &[]));
        b.begin(check);
        let p = b.ptr_add(Flags::NONE, ptr, i);
        let ch = b.load(Ty::I8, Flags::NONE, p, 0, MemAttrs::aligned(1));
        let eq = b.icmp(IntCond::Eq, ch, needle);
        b.br(Flags::NONE, eq, (found, &[]), (step, &[]));
        b.begin(step);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i2]));
        b.begin(found);
        b.ret(&[i]);
        b.begin(not_found);
        b.ret(&[n]);
        let func = b.finish_checked().unwrap();

        let out = vectorize_memchr_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        assert!(
            (0..out.func.view().len()).any(|k| out.func.view().op(k) == Op::ReduceUmax),
            "expected 64B/iter SIMD v16xi8 ReduceUmax (`umaxv.16b`) scan!"
        );
    }
}

//! 128-bit SIMD Loop Auto-Vectorizer with 2x Unrolling (`256 bits / iteration`).
//!
//! Transforms countable unit-stride scalar loops into 2x-unrolled 128-bit vector loops
//! (`<2 x i64>`, `<4 x i32>`, `<8 x i16>`, `<16 x i8>`, `<2 x f64>`, `<4 x f32>`) with:
//!
//! - **Dual Independent 128-bit Accumulators** on associative reductions (`Add`, `Xor`, `Or`,
//!   `And`, and `REASSOC` `Fadd`), breaking loop-carried ALU latency chains on out-of-order cores.
//! - **Adjacent 16-Byte Vector Loads & Stores** (`[ptr, off]` and `[ptr, off + 16]`), fusing
//!   directly into 32-byte `ldp q, q` / `stp q, q` instructions in AArch64 `TierName::Sota` and
//!   unrolled `movdqu` pairs in x86-64 `TierName::Sota`.
//! - **Sound Runtime Aliasing Guard** on unit-stride map/transform store loops (`dst == src`
//!   in-place elementwise OR `dst_end <= src_start || src_end <= dst_start`), falling back to the
//!   original scalar loop if pointers partially overlap.
//! - **Exact Scalar Epilogue**: remaining `< 2 * lanes` elements finish in the untouched scalar
//!   loop for bit-exact results across every length (`0, 1, 2, ...`).
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

/// Telemetry returned by [`auto_vectorize_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct VectorizeStats {
    /// Number of countable scalar loops transformed into 2x-unrolled 128-bit SIMD loops.
    pub loops_vectorized: u32,
}

/// Output of [`auto_vectorize_loops`].
#[derive(Debug)]
pub struct VectorizeOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: VectorizeStats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoopShape {
    /// `header` checks `i < n` -> `body` -> `Jump(header)`, `header` -> `exit`.
    HeaderBody { header: u32, body: u32, exit: u32 },
    /// Single-block do-while: `header` executes body, computes `i_next = i + 1`, checks `i_next < n` -> `header` | `exit`.
    SingleBlock { header: u32, exit: u32 },
}

#[derive(Clone, Copy, Debug)]
struct ReductionSpec {
    /// Position of this accumulator in `header`'s block parameter list.
    param_pos: usize,
    /// The `header` block param instruction index.
    param_inst: u32,
    /// Initial value passed from `preheader`.
    init_val: u32,
    /// Reduction op (`Add`, `Xor`, `Or`, `And`, `Fadd`).
    op: Op,
    /// Flags on the reduction op (e.g. `Flags::REASSOC` for `Fadd`).
    flags: Flags,
    /// The instruction computing `acc_next = op(acc, rhs)` in the loop body.
    update_inst: u32,
    /// The element expression operand (`rhs`, the non-`acc` operand).
    elem_val: u32,
}

#[derive(Clone, Copy, Debug)]
struct UnitStrideMem {
    /// Original `Op::Load` or `Op::Store` instruction index.
    inst: u32,
    /// Loop-invariant base pointer value.
    base: u32,
    /// Optional loop-invariant element-index offset `row_off` (e.g. `r * cols` after 2D loop
    /// interchange!) for addresses of the form `base + (row_off + i) * elem_bytes`.
    row_idx_off: Option<u32>,
    /// Constant byte offset added to `base + (row_off + i) * elem_bytes`.
    const_off: i32,
    /// Stored value (for `Op::Store` only; `NONE` for `Op::Load`).
    store_val: u32,
    /// Original memory attributes index in `view.mems`.
    mem_attr: u32,
}

struct VecLoopPlan<A: Allocator> {
    preheader: u32,
    shape: LoopShape,
    /// Position of the induction variable `i` in `header`'s block parameters.
    iv_pos: usize,
    /// Induction variable instruction (`header` param).
    iv_inst: u32,
    /// Induction variable type (`Ty::I64` or `Ty::I32`).
    iv_ty: Ty,
    /// Initial value of `i` passed from `preheader`.
    iv_init: u32,
    /// Loop-invariant upper bound `n` (`i < n` unsigned).
    bound: u32,
    /// Scalar element type (`I8`, `I16`, `I32`, `I64`, `F32`, `F64`).
    elem_ty: Ty,
    /// 128-bit vector type (`<lanes x elem_ty>`).
    vec_ty: Ty,
    /// `log2(elem_bytes)` (`0, 1, 2, 3`).
    elem_shift: u32,
    /// Number of lanes per 128-bit vector (`16, 8, 4, 2`).
    lanes: u32,
    /// Original `preheader` jump arguments to `header`.
    pre_args: Vec<u32, A>,
    /// Reduction accumulators (0 for pure map/store loops, >= 1 for reduction loops).
    reductions: Vec<ReductionSpec, A>,
    /// Unit-stride loads in body order.
    loads: Vec<UnitStrideMem, A>,
    /// Optional single unit-stride store (for map/transform loops).
    store: Option<UnitStrideMem>,
    /// All pure vectorizable body instructions (in topological body order) needed to compute
    /// `elem_val`s / `store_val`, excluding `loads` and `reductions`.
    vec_exprs: Vec<u32, A>,
    /// Loop-invariant scalar values referenced by `vec_exprs` that must be splatted in `vec_pre`.
    inv_scalars: Vec<u32, A>,
    /// Distinct loop-invariant base pointers of loads (for runtime overlap check against `store`).
    load_bases: Vec<u32, A>,
    /// Original `SingleBlock` exit arguments (referencing `header` defs) for direct exit when tail is empty.
    single_exit_args: Vec<u32, A>,
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

#[inline]
fn supported_elem(ty: Ty, vec_bytes: u32) -> Option<(Ty, u32, u32)> {
    if !ty.is_scalar() || !matches!(vec_bytes, 16 | 32 | 64) {
        return None;
    }
    let elem_shift = if ty == Ty::I8 {
        0u32
    } else if ty == Ty::I16 {
        1
    } else if ty == Ty::I32 || ty == Ty::F32 {
        2
    } else if ty == Ty::I64 || ty == Ty::F64 {
        3
    } else {
        return None;
    };
    let lanes = vec_bytes >> elem_shift;
    Some((Ty::vector(ty.elem(), lanes)?, elem_shift, lanes))
}

/// Checks whether `ptr_val` computes `base + (row_off + iv) * elem_bytes + delta` where `base`
/// (and optional `row_off`) are loop-invariant.
fn match_unit_stride_addr(
    view: &View<'_>,
    ptr_val: u32,
    extra_off: i32,
    iv_inst: u32,
    elem_shift: u32,
    is_loop_block: impl Fn(u32) -> bool,
    inst_block: &[u32],
) -> Option<(u32, i32, Option<u32>)> {
    let pi = to_idx(ptr_val);
    if view.op(pi) != Op::PtrAdd {
        return None;
    }
    let idx = to_idx(at(view.data, pi));
    let base = at(view.extra, idx);
    let by = at(view.extra, idx + 1);
    if is_loop_block(at(inst_block, to_idx(base))) {
        return None;
    }
    let is_iv = |v: u32| -> bool {
        let vi = to_idx(v);
        v == iv_inst || (view.op(vi) == Op::Zext && at(view.data, vi) == iv_inst)
    };
    let match_iv_plus_inv = |v: u32| -> Option<Option<u32>> {
        if is_iv(v) {
            return Some(None);
        }
        let vi = to_idx(v);
        if view.op(vi) == Op::Add && Ty(at(view.tys, vi)) == Ty::I64 {
            let aat = to_idx(at(view.data, vi));
            let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
            if is_iv(al) && !is_loop_block(at(inst_block, to_idx(ar))) && Ty(at(view.tys, to_idx(ar))) == Ty::I64 {
                return Some(Some(ar));
            }
            if is_iv(ar) && !is_loop_block(at(inst_block, to_idx(al))) && Ty(at(view.tys, to_idx(al))) == Ty::I64 {
                return Some(Some(al));
            }
        }
        None
    };
    let bi = to_idx(by);
    let elem_bytes = 1u64 << elem_shift;
    let row_off = if elem_shift == 0 {
        if let Some(ro) = match_iv_plus_inv(by) {
            Some(ro)
        } else if view.op(bi) == Op::Mul {
            let mat = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, mat), at(view.extra, mat + 1));
            if const_u64(view, to_idx(r)) == Some(1) {
                match_iv_plus_inv(l)
            } else if const_u64(view, to_idx(l)) == Some(1) {
                match_iv_plus_inv(r)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        match view.op(bi) {
            Op::Shl => {
                let sat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, sat), at(view.extra, sat + 1));
                if const_u64(view, to_idx(r)) == Some(u64::from(elem_shift)) {
                    match_iv_plus_inv(l)
                } else {
                    None
                }
            }
            Op::Mul => {
                let mat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, mat), at(view.extra, mat + 1));
                if const_u64(view, to_idx(r)) == Some(elem_bytes) {
                    match_iv_plus_inv(l)
                } else if const_u64(view, to_idx(l)) == Some(elem_bytes) {
                    match_iv_plus_inv(r)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    row_off.map(|ro| (base, extra_off, ro))
}

/// Checks if an instruction is a pure 128-bit vectorizable op for `elem_ty` on BOTH AArch64 NEON
/// and x86-64 SSE2 (`Mul` on `i64` is excluded since neither NEON nor SSE2 has native 64x64->64 vector mul).
#[inline]
fn is_vec_alu_op(op: Op, elem_ty: Ty) -> bool {
    if elem_ty.is_float() {
        matches!(op, Op::Fadd | Op::Fsub | Op::Fmul)
    } else {
        match op {
            Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => true,
            Op::Mul => elem_ty == Ty::I16 || elem_ty == Ty::I32 || elem_ty == Ty::I8,
            _ => false,
        }
    }
}

/// Checks if `op` is a supported associative reduction op for `elem_ty`.
#[inline]
fn is_reduction_op(op: Op, flags: Flags, elem_ty: Ty) -> bool {
    if elem_ty.is_float() {
        op == Op::Fadd && flags.contains(Flags::REASSOC)
    } else {
        matches!(op, Op::Add | Op::Xor | Op::Or | Op::And)
    }
}

fn find_vectorizable_loop<A: Allocator + Copy>(
    view: &View<'_>,
    vec_bytes: u32,
    alloc: A,
) -> Option<VecLoopPlan<A>> {
    if view.head.ptr_bits != 64 {
        return None;
    }
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return None;
    }

    // Build block ownership & predecessor lists.
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

    // Scan for a countable natural loop with a unique preheader ending in `Op::Jump`.
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        // Identify `(preheader, latch)` where `preheader < header <= latch`.
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
        if h_params.is_empty() {
            continue;
        }
        let pre_argc = to_idx(at(view.extra, pre_at + 1));
        if pre_argc != h_params.len() {
            continue;
        }
        let mut pre_args: Vec<u32, A> = Vec::with_capacity_in(pre_argc, alloc);
        for k in 0..pre_argc {
            pre_args.push(at(view.extra, pre_at + 2 + k));
        }

        // Determine loop shape (SingleBlock or HeaderBody).
        let (shape, body_block, back_args_start, cond_val, single_exit_args) = if latch == header {
            // Single-block loop: `header` ends in `Br(cond, (header, back_args), (exit, exit_args))`.
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
            (LoopShape::SingleBlock { header, exit: e_blk }, header, idx + 3, cv, ex_args)
        } else {
            // Two-block loop: `header` ends in `Br(cond, (body, []), (exit, ...))`, `body` has 1 pred (`header`) and ends in `Jump(header, back_args)`.
            if preds.get(to_idx(latch)).map(Vec::as_slice) != Some(&[header]) {
                continue;
            }
            if !view.params(Block(latch)).is_empty() {
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
            // Header must only compute the exit compare before `Br`.
            let h_range = view.block(Block(header));
            let mut header_simple = true;
            for inst in h_params.end..(h_range.end - 1) {
                if inst != to_idx(cv) {
                    header_simple = false;
                    break;
                }
            }
            if !header_simple {
                continue;
            }
            (
                LoopShape::HeaderBody { header, body: latch, exit: e_blk },
                latch,
                bat + 2,
                cv,
                Vec::new_in(alloc),
            )
        };

        let is_loop_blk = |b: u32| b == header || b == body_block;

        // Parse exit condition `icmp(Ult, lhs, bound)` where `bound` is loop-invariant.
        let ci = to_idx(cond_val);
        if view.op(ci) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, ci));
        if IntCond::from_word(at(view.extra, cat)) != Some(IntCond::Ult) {
            continue;
        }
        let cmp_lhs = at(view.extra, cat + 1);
        let bound = at(view.extra, cat + 2);
        if is_loop_blk(at(&inst_block, to_idx(bound))) {
            continue;
        }

        // Identify the unit-step induction variable `i` (`i_next = add(i, 1)`).
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
            let matches_cmp = match shape {
                LoopShape::HeaderBody { .. } => cmp_lhs == p_inst,
                LoopShape::SingleBlock { .. } => cmp_lhs == back_v,
            };
            if matches_cmp {
                found_iv = Some((pos, p_inst, back_v, p_ty));
                break;
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst, iv_ty)) = found_iv else {
            continue;
        };

        // Ensure no loop-body instruction escapes to outside blocks except via exit edge arguments.
        let mut used_outside = filled(len, false, &alloc);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        set_at(&mut used_outside, to_idx(v), true);
                    }
                });
            }
        }
        let mut has_illegal_escape = false;
        for blk in [header, body_block] {
            for inst in view.block(Block(blk)) {
                if at(&used_outside, inst) {
                    // Only header params used directly in exit (Shape 1) are allowed if they're IV or reductions.
                    if !(blk == header && inst >= h_params.start && inst < h_params.end) {
                        has_illegal_escape = true;
                    }
                }
            }
        }
        if has_illegal_escape {
            continue;
        }

        // Scan body instructions: collect loads, optional single store, reductions, and check purity.
        let body_range = view.block(Block(body_block));
        let body_start = if body_block == header { h_params.end } else { body_range.start };
        let body_end = body_range.end - 1; // exclude terminator

        let mut elem_ty: Option<Ty> = None;
        let mut vec_ty_info: Option<(Ty, u32, u32)> = None;
        let mut loads: Vec<UnitStrideMem, A> = Vec::new_in(alloc);
        let mut store: Option<UnitStrideMem> = None;
        let mut bad_body = false;

        // First pass: determine common `elem_ty` from loads/stores/reductions and reject side effects.
        for inst in body_start..body_end {
            let op = view.op(inst);
            let flags = Flags(at(view.flags, inst));
            match op {
                Op::Load => {
                    if flags.contains(Flags::VOLATILE) {
                        bad_body = true;
                        break;
                    }
                    let lt = Ty(at(view.tys, inst));
                    let Some(info) = supported_elem(lt, vec_bytes) else {
                        bad_body = true;
                        break;
                    };
                    if let Some(prev) = elem_ty {
                        if prev != lt {
                            bad_body = true;
                            break;
                        }
                    } else {
                        elem_ty = Some(lt);
                        vec_ty_info = Some(info);
                    }
                }
                Op::Store => {
                    if flags.contains(Flags::VOLATILE) || store.is_some() {
                        bad_body = true;
                        break;
                    }
                    let idx = to_idx(at(view.data, inst));
                    let st = Ty(at(view.tys, to_idx(at(view.extra, idx))));
                    let Some(info) = supported_elem(st, vec_bytes) else {
                        bad_body = true;
                        break;
                    };
                    if let Some(prev) = elem_ty {
                        if prev != st {
                            bad_body = true;
                            break;
                        }
                    } else {
                        elem_ty = Some(st);
                        vec_ty_info = Some(info);
                    }
                }
                Op::Call
                | Op::CallIndirect
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::Trap
                | Op::Udiv
                | Op::Sdiv
                | Op::Urem
                | Op::Srem => {
                    bad_body = true;
                    break;
                }
                _ => {}
            }
        }
        if bad_body {
            continue;
        }
        let (Some(elem_ty), Some((vec_ty, elem_shift, lanes))) = (elem_ty, vec_ty_info) else {
            continue;
        };

        // Match every non-IV header parameter as an associative reduction accumulator.
        let mut reductions: Vec<ReductionSpec, A> = Vec::new_in(alloc);
        let mut is_reduction_update = filled(len, false, &alloc);
        let mut bad_params = false;
        for pos in 0..h_params.len() {
            if pos == iv_pos {
                continue;
            }
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let p_ty = Ty(at(view.tys, p_idx));
            if p_ty != elem_ty {
                bad_params = true;
                break;
            }
            let back_v = at(view.extra, back_args_start + pos);
            let bi = to_idx(back_v);
            let bop = view.op(bi);
            let bflags = Flags(at(view.flags, bi));
            if !is_reduction_op(bop, bflags, elem_ty) || at(&inst_block, bi) != body_block {
                bad_params = true;
                break;
            }
            let bat = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
            let elem_val = if l == p_inst {
                r
            } else if r == p_inst {
                l
            } else {
                bad_params = true;
                break;
            };
            // Ensure the accumulator param is ONLY used by its own reduction update!
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
                bad_params = true;
                break;
            }
            set_at(&mut is_reduction_update, bi, true);
            reductions.push(ReductionSpec {
                param_pos: pos,
                param_inst: p_inst,
                init_val: at(&pre_args, pos),
                op: bop,
                flags: bflags,
                update_inst: back_v,
                elem_val,
            });
        }
        if bad_params {
            continue;
        }
        // Must have either >= 1 reduction OR 1 unit-stride store (not mixed for now).
        if reductions.is_empty() && store.is_none() {
            // Check if a store actually exists below; we haven't populated `store` yet.
        }

        // Match every load/store address as unit-stride `base + i * elem_bytes + const_off`.
        let mut load_bases: Vec<u32, A> = Vec::new_in(alloc);
        let mut is_mem_inst = filled(len, false, &alloc);
        for inst in body_start..body_end {
            let op = view.op(inst);
            if op == Op::Load {
                let idx = to_idx(at(view.data, inst));
                let ptr = at(view.extra, idx);
                let off = at(view.extra, idx + 1).cast_signed();
                let Some((base, const_off, row_idx_off)) =
                    match_unit_stride_addr(view, ptr, off, iv_inst, elem_shift, is_loop_blk, &inst_block)
                else {
                    bad_body = true;
                    break;
                };
                if load_bases.scan_for(&base).is_none() {
                    load_bases.push(base);
                }
                set_at(&mut is_mem_inst, inst, true);
                loads.push(UnitStrideMem {
                    inst: to_u32(inst),
                    base,
                    row_idx_off,
                    const_off,
                    store_val: NONE,
                    mem_attr: at(view.attrs, inst),
                });
            } else if op == Op::Store {
                let idx = to_idx(at(view.data, inst));
                let sv = at(view.extra, idx);
                let ptr = at(view.extra, idx + 1);
                let off = at(view.extra, idx + 2).cast_signed();
                let Some((base, const_off, None)) =
                    match_unit_stride_addr(view, ptr, off, iv_inst, elem_shift, is_loop_blk, &inst_block)
                else {
                    bad_body = true;
                    break;
                };
                set_at(&mut is_mem_inst, inst, true);
                store = Some(UnitStrideMem {
                    inst: to_u32(inst),
                    base,
                    row_idx_off: None,
                    const_off,
                    store_val: sv,
                    mem_attr: at(view.attrs, inst),
                });
            }
        }
        if bad_body || loads.is_empty() || (reductions.is_empty() && store.is_none()) || (!reductions.is_empty() && store.is_some()) {
            continue;
        }
        // Only vectorize SingleBlock loops when all exit args are either outside values or reduction updates.
        if matches!(shape, LoopShape::SingleBlock { .. }) {
            let mut exit_ok = true;
            for &ea in &single_exit_args {
                let ei = to_idx(ea);
                if is_loop_blk(at(&inst_block, ei)) && !at(&is_reduction_update, ei) {
                    exit_ok = false;
                    break;
                }
            }
            if !exit_ok {
                continue;
            }
        }

        // Walk backward from reduction `elem_val`s (or `store.store_val`) to collect `vec_exprs`
        // and verify every intermediate op is a supported vector ALU op of `elem_ty`!
        let mut needed = filled(len, false, &alloc);
        let mut stack: Vec<u32, A> = Vec::new_in(alloc);
        for red in &reductions {
            stack.push(red.elem_val);
        }
        if let Some(st) = store {
            stack.push(st.store_val);
        }
        let mut inv_scalars: Vec<u32, A> = Vec::new_in(alloc);
        let mut expr_bad = false;
        while let Some(v) = stack.pop() {
            let vi = to_idx(v);
            if at(&needed, vi) {
                continue;
            }
            if !is_loop_blk(at(&inst_block, vi)) {
                if Ty(at(view.tys, vi)) != elem_ty {
                    expr_bad = true;
                    break;
                }
                if inv_scalars.scan_for(&v).is_none() {
                    inv_scalars.push(v);
                }
                continue;
            }
            if at(&is_mem_inst, vi) {
                set_at(&mut needed, vi, true);
                continue;
            }
            if v == iv_inst || v == iv_next_inst {
                // Direct use of IV in data expression (not just address): skip for now.
                expr_bad = true;
                break;
            }
            let op = view.op(vi);
            if matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
                if Ty(at(view.tys, vi)) != elem_ty {
                    expr_bad = true;
                    break;
                }
                set_at(&mut needed, vi, true);
                continue;
            }
            if op == Op::Icmp {
                let idx = to_idx(at(view.data, vi));
                let (a_u32, bv_u32) = (at(view.extra, idx + 1), at(view.extra, idx + 2));
                if elem_ty.is_float()
                    || Ty(at(view.tys, to_idx(a_u32))) != elem_ty
                    || Ty(at(view.tys, to_idx(bv_u32))) != elem_ty
                {
                    expr_bad = true;
                    break;
                }
                set_at(&mut needed, vi, true);
                stack.push(a_u32);
                stack.push(bv_u32);
                continue;
            }
            if op == Op::Select {
                let idx = to_idx(at(view.data, vi));
                let (cv_u32, tv_u32, fv_u32) = (
                    at(view.extra, idx),
                    at(view.extra, idx + 1),
                    at(view.extra, idx + 2),
                );
                let cv = to_idx(cv_u32);
                if Ty(at(view.tys, vi)) != elem_ty
                    || Ty(at(view.tys, to_idx(tv_u32))) != elem_ty
                    || Ty(at(view.tys, to_idx(fv_u32))) != elem_ty
                    || !is_loop_blk(at(&inst_block, cv))
                    || view.op(cv) != Op::Icmp
                {
                    expr_bad = true;
                    break;
                }
                set_at(&mut needed, vi, true);
                stack.push(cv_u32);
                stack.push(tv_u32);
                stack.push(fv_u32);
                continue;
            }
            if Ty(at(view.tys, vi)) != elem_ty || !is_vec_alu_op(op, elem_ty) {
                expr_bad = true;
                break;
            }
            set_at(&mut needed, vi, true);
            each(view, vi, |item| {
                if let Item::Value(opnd) = item {
                    stack.push(opnd);
                }
            });
        }
        if expr_bad {
            continue;
        }

        let mut vec_exprs: Vec<u32, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            if at(&needed, inst) && !at(&is_mem_inst, inst) {
                vec_exprs.push(to_u32(inst));
            }
        }

        // For map/store loops, check that all load bases have the same constant offset as the store
        // when `base == dst_base` (in-place `a[i] = a[i] ^ k` is safe; shifted `a[i] = a[i-1]` is not!).
        if let Some(st) = store {
            let mut alias_unsafe = false;
            for ld in &loads {
                if ld.base == st.base && ld.const_off != st.const_off {
                    alias_unsafe = true;
                    break;
                }
            }
            if alias_unsafe {
                continue;
            }
        }

        let iv_init = at(&pre_args, iv_pos);
        return Some(VecLoopPlan {
            preheader,
            shape,
            iv_pos,
            iv_inst,
            iv_ty,
            iv_init,
            bound,
            elem_ty,
            vec_ty,
            elem_shift,
            lanes,
            pre_args,
            reductions,
            loads,
            store,
            vec_exprs,
            inv_scalars,
            load_bases,
            single_exit_args,
        });
    }

    None
}

// Low-level LIR instruction emission helpers directly on `Func<A>`.
fn push_inst<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    data: u32,
    attr: u32,
) -> u32 {
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
    let bits = ty.bits(64);
    if bits <= 32 {
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

fn push_fconst_zero<A: Allocator>(func: &mut Func<A>, ty: Ty) -> u32 {
    if ty == Ty::F32 {
        push_inst(func, Op::Fconst, Flags::NONE, Ty::F32, 0, 0)
    } else {
        let idx = to_u32(func.extra.len());
        func.extra.push(0);
        func.extra.push(0);
        push_inst(func, Op::FconstWide, Flags::NONE, Ty::F64, idx, 0)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, idx, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, idx, 0)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, idx, 0)
}

fn push_br<A: Allocator>(func: &mut Func<A>, cond: u32, t_blk: u32, t_args: &[u32], e_blk: u32, e_args: &[u32]) -> u32 {
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
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, idx, 0)
}

fn copy_inst_remapped_blocks<A: Allocator>(
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

/// Automatically vectorizes eligible countable unit-stride scalar loops into 4x-unrolled 128-bit
/// SIMD loops (`64 bytes / iteration`) with 4 independent accumulators and an exact scalar tail.
pub fn auto_vectorize_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    auto_vectorize_loops_with_width(view, 16, alloc)
}

/// Automatically vectorizes eligible countable unit-stride scalar loops into 4x-unrolled
/// `vec_bytes`-wide SIMD loops (`16` = 128-bit NEON/SSE2 -> `64 B/iter`; `32` = 256-bit AVX2 -> `128 B/iter`).
pub fn auto_vectorize_loops_with_width<A: Allocator + Copy>(
    view: View<'_>,
    vec_bytes: u32,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    let Some(plan) = find_vectorizable_loop(&view, vec_bytes, alloc) else {
        return Ok(VectorizeOutput {
            func: view.to_func_in(alloc),
            stats: VectorizeStats::default(),
        });
    };

    let blocks = view.blocks();
    let len = view.len();
    // 4x unrolling = 64 bytes (1 full Apple Silicon / x86-64 cache line) per iteration!
    // Emits TWO `ldp q, q` pairs (`+0, +16` and `+32, +48`) and 4 independent 128-bit
    // accumulators to saturate all 4 NEON ALU pipes and dual 128-bit load units.
    const UNROLL: usize = 4;
    let unroll_step = u64::from(plan.lanes) * u64::try_from(UNROLL).unwrap_or(4);

    // Insert 3 new blocks (`vec_pre`, `vec_loop`, `vec_reduce`) immediately after `plan.preheader`!
    let insert_at = to_idx(plan.preheader) + 1;
    let vec_pre_blk = to_u32(insert_at);
    let vec_loop_blk = to_u32(insert_at + 1);
    let vec_reduce_blk = to_u32(insert_at + 2);

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + 3) });
    }
    let orig_header_new = match plan.shape {
        LoopShape::HeaderBody { header, .. } | LoopShape::SingleBlock { header, .. } => {
            at(&blk_map, to_idx(header))
        }
    };
    let orig_exit_new = match plan.shape {
        LoopShape::HeaderBody { exit, .. } | LoopShape::SingleBlock { exit, .. } => {
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

    // Reuse the scalar load's element-alignment `mem_attrs` index for unaligned 128-bit vector accesses.
    let vec_mem_attr = plan.loads.first().map_or(0, |ld| ld.mem_attr);

    let mut val_map = filled(len, NONE, &alloc);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            // Emit vectorization guard at end of `preheader`.
            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let bound_new = at(&val_map, to_idx(plan.bound));
            let has_rem = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
            let rem_val = push_bin(&mut func, Op::Sub, Flags::NONE, plan.iv_ty, bound_new, iv_init_new);
            let k_step = push_iconst(&mut func, plan.iv_ty, unroll_step);
            let enough = push_icmp(&mut func, IntCond::Uge, rem_val, k_step);
            let mut go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);

            // For map/store loops, emit sound runtime non-overlap alias check against every distinct load base!
            if let Some(st) = plan.store {
                let dst_b = at(&val_map, to_idx(st.base));
                let dst_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, dst_b, 0);
                let bound64 = if plan.iv_ty == Ty::I32 {
                    push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, bound_new, 0)
                } else {
                    bound_new
                };
                let iv0_64 = if plan.iv_ty == Ty::I32 {
                    push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv_init_new, 0)
                } else {
                    iv_init_new
                };
                let start_bytes = if plan.elem_shift == 0 {
                    iv0_64
                } else {
                    let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                    push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, iv0_64, ks)
                };
                let end_bytes = if plan.elem_shift == 0 {
                    bound64
                } else {
                    let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                    push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, bound64, ks)
                };
                let k_off = push_iconst(&mut func, Ty::I64, i64::from(st.const_off).cast_unsigned());
                let dst_base_off = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, dst_addr, k_off);
                let dst_lo = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, dst_base_off, start_bytes);
                let dst_hi = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, dst_base_off, end_bytes);
                for ld in &plan.loads {
                    if ld.base == st.base && ld.const_off == st.const_off {
                        continue; // in-place elementwise `a[i] = f(a[i])` verified offset-identical above!
                    }
                    let src_b = at(&val_map, to_idx(ld.base));
                    let src_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, src_b, 0);
                    let src_base_off = if ld.const_off != 0 {
                        let kl = push_iconst(&mut func, Ty::I64, i64::from(ld.const_off).cast_unsigned());
                        push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, src_addr, kl)
                    } else {
                        src_addr
                    };
                    let src_lo = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, src_base_off, start_bytes);
                    let src_hi = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, src_base_off, end_bytes);
                    let eq_same = push_icmp(&mut func, IntCond::Eq, dst_lo, src_lo);
                    let before = push_icmp(&mut func, IntCond::Ule, dst_hi, src_lo);
                    let after = push_icmp(&mut func, IntCond::Ule, src_hi, dst_lo);
                    let disj = push_bin(&mut func, Op::Or, Flags::NONE, Ty::I1, before, after);
                    let safe = push_bin(&mut func, Op::Or, Flags::NONE, Ty::I1, eq_same, disj);
                    go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, go_vec, safe);
                }
            }

            let mut fallback_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for &a in &plan.pre_args {
                fallback_args.push(at(&val_map, to_idx(a)));
            }
            push_br(&mut func, go_vec, vec_pre_blk, &[], orig_header_new, &fallback_args);

            // --- Emit `vec_pre` block ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mask_val = !(unroll_step - 1);
            let k_mask = push_iconst(&mut func, plan.iv_ty, mask_val);
            let vec_count = push_bin(&mut func, Op::And, Flags::NONE, plan.iv_ty, rem_val, k_mask);
            let vec_end = push_bin(&mut func, Op::Add, Flags::NONE, plan.iv_ty, iv_init_new, vec_count);

            let mut vec_init_args: Vec<u32, A> = Vec::with_capacity_in(1 + plan.reductions.len() * UNROLL, alloc);
            vec_init_args.push(iv_init_new);
            for red in &plan.reductions {
                let id_scalar = if plan.elem_ty.is_float() {
                    push_fconst_zero(&mut func, plan.elem_ty)
                } else if red.op == Op::And {
                    push_iconst(&mut func, plan.elem_ty, u64::MAX)
                } else {
                    push_iconst(&mut func, plan.elem_ty, 0)
                };
                for _ in 0..UNROLL {
                    let vacc = push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, id_scalar, 0);
                    vec_init_args.push(vacc);
                }
            }
            // Hoist invariant scalar splats and 2D row-offset base pointers once into `vec_pre`!
            let mut inv_splat_map: Vec<(u32, u32), A> = Vec::with_capacity_in(plan.inv_scalars.len(), alloc);
            for &sv in &plan.inv_scalars {
                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, at(&val_map, to_idx(sv)), 0);
                inv_splat_map.push((sv, sp));
            }
            let mut load_eff_bases: Vec<u32, A> = Vec::with_capacity_in(plan.loads.len(), alloc);
            for ld in &plan.loads {
                let b_new = at(&val_map, to_idx(ld.base));
                let eff = if let Some(row_off) = ld.row_idx_off {
                    let row_v = at(&val_map, to_idx(row_off));
                    let row_bytes = if plan.elem_shift == 0 {
                        row_v
                    } else {
                        let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                        push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, row_v, ks)
                    };
                    push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, b_new, row_bytes)
                } else {
                    b_new
                };
                load_eff_bases.push(eff);
            }
            push_jump(&mut func, vec_loop_blk, &vec_init_args);

            // --- Emit `vec_loop` block (4x-unrolled 128-bit SIMD = 64 bytes / 512 bits per iter!) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_v = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0, 0);
            let mut vacc_params: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for _ in 0..plan.reductions.len() {
                let a0 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let a1 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let a2 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let a3 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                vacc_params.push([a0, a1, a2, a3]);
            }

            // Shared byte offset: `iv_v << elem_shift`.
            let iv_v64 = if plan.iv_ty == Ty::I32 {
                push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv_v, 0)
            } else {
                iv_v
            };
            let byte_off = if plan.elem_shift == 0 {
                iv_v64
            } else {
                let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, iv_v64, ks)
            };

            // Emit FOUR adjacent 16-byte vector loads (`+0,+16` and `+32,+48` -> TWO `ldp q, q` = 64B!).
            let mut vmaps: [Vec<u32, A>; UNROLL] = [
                filled(len, NONE, &alloc),
                filled(len, NONE, &alloc),
                filled(len, NONE, &alloc),
                filled(len, NONE, &alloc),
            ];
            for &(sv, sp) in &inv_splat_map {
                for vm in &mut vmaps {
                    set_at(vm, to_idx(sv), sp);
                }
            }
            let vec_bytes_i32 = vec_bytes.cast_signed();
            for (lidx, ld) in plan.loads.iter().enumerate() {
                let base_new = at(&load_eff_bases, lidx);
                let vptr = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, base_new, byte_off);
                for (u, vm) in vmaps.iter_mut().enumerate() {
                    let idx = to_u32(func.extra.len());
                    func.extra.push(vptr);
                    let u_i32 = i32::try_from(u).unwrap_or(0);
                    func.extra.push(ld.const_off.wrapping_add(u_i32.wrapping_mul(vec_bytes_i32)).cast_unsigned());
                    let q = push_inst(&mut func, Op::Load, Flags::NONE, plan.vec_ty, idx, vec_mem_attr);
                    set_at(vm, to_idx(ld.inst), q);
                }
            }

            // Vectorize body expressions across all 4 unrolled lanes (including masked `Icmp` & `Select`!).
            let mask_ty = plan.vec_ty.with_elem(crate::ty::Scalar::I1);
            for &ei in &plan.vec_exprs {
                let e = to_idx(ei);
                let op = view.op(e);
                let fl = Flags(at(view.flags, e));
                if matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
                    let c_new = copy_inst_remapped_blocks(&view, e, &val_map, &blk_map, &mut func);
                    let sp = push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, c_new, 0);
                    for vm in &mut vmaps {
                        set_at(vm, e, sp);
                    }
                } else if op == Op::Icmp {
                    let idx = to_idx(at(view.data, e));
                    let cond_w = at(view.extra, idx);
                    let (a, bv) = (to_idx(at(view.extra, idx + 1)), to_idx(at(view.extra, idx + 2)));
                    for vm in &mut vmaps {
                        let dst = to_u32(func.extra.len());
                        func.extra.push(cond_w);
                        func.extra.push(at(vm, a));
                        func.extra.push(at(vm, bv));
                        let nv = push_inst(&mut func, Op::Icmp, fl, mask_ty, dst, 0);
                        set_at(vm, e, nv);
                    }
                } else if op == Op::Select {
                    let idx = to_idx(at(view.data, e));
                    let (cv, tv, fv) = (
                        to_idx(at(view.extra, idx)),
                        to_idx(at(view.extra, idx + 1)),
                        to_idx(at(view.extra, idx + 2)),
                    );
                    for vm in &mut vmaps {
                        let dst = to_u32(func.extra.len());
                        func.extra.push(at(vm, cv));
                        func.extra.push(at(vm, tv));
                        func.extra.push(at(vm, fv));
                        let nv = push_inst(&mut func, Op::Select, fl, plan.vec_ty, dst, 0);
                        set_at(vm, e, nv);
                    }
                } else {
                    let idx = to_idx(at(view.data, e));
                    let (a, bv) = (to_idx(at(view.extra, idx)), to_idx(at(view.extra, idx + 1)));
                    for vm in &mut vmaps {
                        let nv = push_bin(&mut func, op, fl, plan.vec_ty, at(vm, a), at(vm, bv));
                        set_at(vm, e, nv);
                    }
                }
            }

            // Emit FOUR adjacent `vec_bytes` vector stores (`+0,+16,+32,+48` on 128b; `+0,+32,+64,+96` on 256b!).
            if let Some(st) = plan.store {
                let dst_new = at(&val_map, to_idx(st.base));
                let dptr = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, dst_new, byte_off);
                for (u, vm) in vmaps.iter().enumerate() {
                    let sv = at(vm, to_idx(st.store_val));
                    let idx = to_u32(func.extra.len());
                    func.extra.push(sv);
                    func.extra.push(dptr);
                    let u_i32 = i32::try_from(u).unwrap_or(0);
                    func.extra.push(st.const_off.wrapping_add(u_i32.wrapping_mul(vec_bytes_i32)).cast_unsigned());
                    push_inst(&mut func, Op::Store, Flags::NONE, Ty::NONE, idx, vec_mem_attr);
                }
            }

            // Update 4 independent vector accumulators!
            let k_ustep = push_iconst(&mut func, plan.iv_ty, unroll_step);
            let iv_v_next = push_bin(&mut func, Op::Add, Flags::NONE, plan.iv_ty, iv_v, k_ustep);
            let mut vec_back_args: Vec<u32, A> = Vec::with_capacity_in(1 + plan.reductions.len() * UNROLL, alloc);
            vec_back_args.push(iv_v_next);
            for (ridx, red) in plan.reductions.iter().enumerate() {
                for (u, vm) in vmaps.iter().enumerate() {
                    let acc_u = vacc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                    let n_acc = push_bin(
                        &mut func,
                        red.op,
                        red.flags,
                        plan.vec_ty,
                        acc_u,
                        at(vm, to_idx(red.elem_val)),
                    );
                    vec_back_args.push(n_acc);
                }
            }
            let keep_vec = push_icmp(&mut func, IntCond::Ult, iv_v_next, vec_end);
            push_br(&mut func, keep_vec, vec_loop_blk, &vec_back_args, vec_reduce_blk, &vec_back_args);

            // --- Emit `vec_reduce` block (tree-reduces all 4 accumulators!) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_done = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0, 0);
            let mut red_scalar_vals: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                let d0 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let d1 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let d2 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let d3 = push_inst(&mut func, Op::Param, Flags::NONE, plan.vec_ty, 0, 0);
                let c01 = push_bin(&mut func, red.op, red.flags, plan.vec_ty, d0, d1);
                let c23 = push_bin(&mut func, red.op, red.flags, plan.vec_ty, d2, d3);
                let comb = push_bin(&mut func, red.op, red.flags, plan.vec_ty, c01, c23);
                let init_new = at(&val_map, to_idx(red.init_val));
                let final_s = if red.op == Op::Fadd {
                    push_bin(&mut func, Op::ReduceFadd, red.flags, plan.elem_ty, init_new, comb)
                } else {
                    let red_op = match red.op {
                        Op::Add => Op::ReduceAdd,
                        Op::Xor => Op::ReduceXor,
                        Op::Or => Op::ReduceOr,
                        Op::And => Op::ReduceAnd,
                        _ => {
                            return Err(SimplifyError::Refused {
                                inst: to_idx(red.update_inst),
                                what: "unsupported vector reduction op",
                            });
                        }
                    };
                    let r_lane = push_inst(&mut func, red_op, Flags::NONE, plan.elem_ty, comb, 0);
                    push_bin(&mut func, red.op, Flags::NONE, plan.elem_ty, init_new, r_lane)
                };
                red_scalar_vals.push(final_s);
            }

            let mut tail_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                if pos == plan.iv_pos {
                    tail_args.push(iv_done);
                } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                    tail_args.push(at(&red_scalar_vals, ridx));
                } else {
                    tail_args.push(at(&val_map, to_idx(pre_arg)));
                }
            }
            match plan.shape {
                LoopShape::HeaderBody { .. } => {
                    push_jump(&mut func, orig_header_new, &tail_args);
                }
                LoopShape::SingleBlock { .. } => {
                    let has_tail = push_icmp(&mut func, IntCond::Ult, iv_done, bound_new);
                    let mut direct_exit_args: Vec<u32, A> = Vec::with_capacity_in(plan.single_exit_args.len(), alloc);
                    for &ea in &plan.single_exit_args {
                        if let Some(ridx) = plan.reductions.iter().position(|r| r.update_inst == ea) {
                            direct_exit_args.push(at(&red_scalar_vals, ridx));
                        } else {
                            direct_exit_args.push(at(&val_map, to_idx(ea)));
                        }
                    }
                    push_br(&mut func, has_tail, orig_header_new, &tail_args, orig_exit_new, &direct_exit_args);
                }
            }
        } else {
            for inst in range {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(VectorizeOutput {
        func,
        stats: VectorizeStats { loops_vectorized: 1 },
    })
}

/// Computes exact 64-bit `MulShift` Granlund–Montgomery parameters `(magic, shift)` for divisor `d`.
fn magic_udiv64_mulshift(d: u64) -> Option<(u64, u32)> {
    if d <= 2 || d.is_power_of_two() || d > (u64::MAX / 2) {
        return None;
    }
    let d128 = u128::from(d);
    let max_n = u128::from(u64::MAX);
    let two64 = 1u128 << 64;
    let nc = two64 - (two64 % d128) - 1;
    for shift in 0..63u32 {
        let two_p = 1u128 << (64 + shift);
        let delta = d128 - (two_p % d128);
        let m = (two_p + delta) / d128;
        if m <= max_n
            && let Ok(m64) = u64::try_from(m)
            && nc.checked_mul(delta).is_some_and(|p| p < two_p)
        {
            return Some((m64, shift));
        }
    }
    None
}

/// Computes exact 32-bit `MulShift` Granlund–Montgomery parameters `(magic32, shift32)` for divisor `d`.
fn magic_udiv32_mulshift(d: u64) -> Option<(u32, u32)> {
    if d <= 2 || d.is_power_of_two() || d > (u64::from(u32::MAX) / 2) {
        return None;
    }
    let max_n = u64::from(u32::MAX);
    let two32 = 1u64 << 32;
    let nc = two32 - (two32 % d) - 1;
    for shift in 0..32u32 {
        let two_p = 1u64 << (32 + shift);
        let delta = d - (two_p % d);
        let m = (two_p + delta) / d;
        if m <= max_n
            && let Ok(m32) = u32::try_from(m)
            && nc.checked_mul(delta).is_some_and(|p| p < two_p)
        {
            return Some((m32, shift));
        }
    }
    None
}

/// Recovers `(divisor, magic32, total_rshift)` for a strength-reduced 64-bit unsigned constant division
/// `lshr(umulhi(iv, magic64), sh64)` so any runtime `bound <= u32::MAX` can execute 8-wide or 16-wide in NEON!
fn match_narrow_div_params(magic64: u64, sh64: u32) -> Option<(u64, u32, u32)> {
    if magic64 == 0 || sh64 >= 63 {
        return None;
    }
    let two_p = 1u128 << (64 + sh64);
    let d_u128 = (two_p / u128::from(magic64)) + 1;
    let d = u64::try_from(d_u128).ok()?;
    if magic_udiv64_mulshift(d) != Some((magic64, sh64)) {
        return None;
    }
    let (m32, s32) = magic_udiv32_mulshift(d)?;
    if 32 + s32 >= 64 {
        return None;
    }
    Some((d, m32, 32 + s32))
}

/// Dynamic-range 8-wide (4x `<2 x i64>`) NEON widening-multiply vectorization for unit-step `u64`
/// reduction loops containing strength-reduced constant division (`i / D`, `i % D`).
///
/// Guards dynamically on `bound <= u32::MAX && rem >= 8` (falling back to the untouched 64-bit
/// loop for huge bounds > 4B), turning 1-wide scalar `umulh` bottlenecks into 4-wide NEON `umull.2d`!
pub fn vectorize_iv_div_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    if view.head.ptr_bits != 64 || view.blocks() < 2 {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    }
    let blocks = view.blocks();
    let len = view.len();

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

    struct DivSite {
        umulhi_inst: u32,
        lshr_inst: u32,
        divisor: u64,
        magic32: u32,
        rshift: u32,
    }

    struct IvDivPlan<A: Allocator> {
        preheader: u32,
        header: u32,
        iv_pos: usize,
        iv_inst: u32,
        iv_init: u32,
        bound: u32,
        pre_args: Vec<u32, A>,
        reductions: Vec<ReductionSpec, A>,
        div_sites: Vec<DivSite, A>,
        body_insts: Vec<u32, A>,
    }

    let mut found: Option<IvDivPlan<A>> = None;
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, body_block) = if p0 < header && p1 > header {
            (p0, p1)
        } else if p1 < header && p0 > header {
            (p1, p0)
        } else {
            continue;
        };
        if preds.get(to_idx(body_block)).map(Vec::as_slice) != Some(&[header])
            || !view.params(Block(body_block)).is_empty()
        {
            continue;
        }
        let pre_term = view.terminator(Block(preheader));
        let h_term = view.terminator(Block(header));
        let b_term = view.terminator(Block(body_block));
        if view.op(pre_term) != Op::Jump || view.op(h_term) != Op::Br || view.op(b_term) != Op::Jump {
            continue;
        }
        let pre_at = to_idx(at(view.data, pre_term));
        let hat = to_idx(at(view.data, h_term));
        let bat = to_idx(at(view.data, b_term));
        let h_params = view.params(Block(header));
        if h_params.len() < 2
            || at(view.extra, pre_at) != header
            || to_idx(at(view.extra, pre_at + 1)) != h_params.len()
            || at(view.extra, bat) != header
            || to_idx(at(view.extra, bat + 1)) != h_params.len()
        {
            continue;
        }
        let cond_v = at(view.extra, hat);
        let cond_i = to_idx(cond_v);
        let t_blk = at(view.extra, hat + 1);
        let t_argc = to_idx(at(view.extra, hat + 2));
        let e_blk = at(view.extra, hat + 3 + t_argc);
        if t_blk != body_block || t_argc != 0 || e_blk == header || e_blk == body_block {
            continue;
        }
        let h_range = view.block(Block(header));
        if (h_params.end..(h_range.end - 1)).any(|i| i != cond_i) {
            continue;
        }
        if view.op(cond_i) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, cond_i));
        if !matches!(IntCond::from_word(at(view.extra, cat)), Some(IntCond::Ult | IntCond::Ule)) {
            continue;
        }
        let cmp_lhs = at(view.extra, cat + 1);
        let bound = at(view.extra, cat + 2);
        let is_loop_blk = |b: u32| b == header || b == body_block;
        if is_loop_blk(at(&inst_block, to_idx(bound))) || Ty(at(view.tys, to_idx(bound))) != Ty::I64 {
            continue;
        }

        let mut pre_args = Vec::with_capacity_in(h_params.len(), alloc);
        for k in 0..h_params.len() {
            pre_args.push(at(view.extra, pre_at + 2 + k));
        }

        // Find unit-step `i64` IV.
        let mut found_iv = None;
        for pos in 0..h_params.len() {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            if p_inst != cmp_lhs || Ty(at(view.tys, p_idx)) != Ty::I64 {
                continue;
            }
            let back_v = at(view.extra, bat + 2 + pos);
            let back_i = to_idx(back_v);
            if view.op(back_i) == Op::Add {
                let aat = to_idx(at(view.data, back_i));
                let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
                if (al == p_inst && const_u64(&view, to_idx(ar)) == Some(1))
                    || (ar == p_inst && const_u64(&view, to_idx(al)) == Some(1))
                {
                    found_iv = Some((pos, p_inst, back_v));
                    break;
                }
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst)) = found_iv else {
            continue;
        };

        let body_range = view.block(Block(body_block));
        let body_start = body_range.start;
        let body_end = body_range.end - 1;
        if body_end - body_start == 0 || body_end - body_start > 20 {
            continue;
        }

        // Match reductions (`Add`/`Xor`/`Or`/`And` on `I64`).
        let mut reductions: Vec<ReductionSpec, A> = Vec::new_in(alloc);
        let mut is_red_upd = filled(len, false, &alloc);
        let mut bad = false;
        for pos in 0..h_params.len() {
            if pos == iv_pos {
                continue;
            }
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            if Ty(at(view.tys, p_idx)) != Ty::I64 {
                bad = true;
                break;
            }
            let back_v = at(view.extra, bat + 2 + pos);
            let bi = to_idx(back_v);
            let bop = view.op(bi);
            if !matches!(bop, Op::Add | Op::Xor | Op::Or | Op::And) || at(&inst_block, bi) != body_block {
                bad = true;
                break;
            }
            let aat = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, aat), at(view.extra, aat + 1));
            let elem_val = if l == p_inst { r } else if r == p_inst { l } else { bad = true; break; };
            set_at(&mut is_red_upd, bi, true);
            reductions.push(ReductionSpec {
                param_pos: pos,
                param_inst: p_inst,
                init_val: at(&pre_args, pos),
                op: bop,
                flags: Flags::NONE,
                update_inst: back_v,
                elem_val,
            });
        }
        if bad || reductions.is_empty() {
            continue;
        }

        // Verify no body instruction escapes outside the loop.
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(&view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        if is_loop_blk(at(&inst_block, vi)) && !(vi >= h_params.start && vi < h_params.end) {
                            bad = true;
                        }
                    }
                });
            }
        }
        if bad {
            continue;
        }

        // Match `lshr(umulhi(iv, magic64), sh64)` division sites and validate all other body ops are `<2 x i64>`-vectorizable!
        let mut div_sites: Vec<DivSite, A> = Vec::new_in(alloc);
        let mut is_div_umulhi = filled(len, false, &alloc);
        let mut is_div_lshr = filled(len, false, &alloc);
        for inst in body_start..body_end {
            if view.op(inst) == Op::Lshr && Ty(at(view.tys, inst)) == Ty::I64 {
                let idx = to_idx(at(view.data, inst));
                let src_u32 = at(view.extra, idx);
                let src = to_idx(src_u32);
                let sh_v = to_idx(at(view.extra, idx + 1));
                if view.op(src) == Op::Umulhi && at(&inst_block, src) == body_block {
                    let uat = to_idx(at(view.data, src));
                    let (ul, ur) = (at(view.extra, uat), at(view.extra, uat + 1));
                    let magic = if ul == iv_inst {
                        const_u64(&view, to_idx(ur))
                    } else if ur == iv_inst {
                        const_u64(&view, to_idx(ul))
                    } else {
                        None
                    };
                    if let (Some(m64), Some(sh64)) = (magic, const_u64(&view, sh_v))
                        && let Ok(sh32) = u32::try_from(sh64)
                        && let Some((divisor, magic32, rshift)) = match_narrow_div_params(m64, sh32)
                    {
                        set_at(&mut is_div_umulhi, src, true);
                        set_at(&mut is_div_lshr, inst, true);
                        div_sites.push(DivSite {
                            umulhi_inst: src_u32,
                            lshr_inst: to_u32(inst),
                            divisor,
                            magic32,
                            rshift,
                        });
                    }
                }
            }
        }
        if div_sites.is_empty() {
            continue;
        }

        let iv_next_idx = to_idx(iv_next_inst);
        let mut body_insts: Vec<u32, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            if inst == iv_next_idx || at(&is_red_upd, inst) || at(&is_div_umulhi, inst) {
                continue;
            }
            if at(&is_div_lshr, inst) {
                body_insts.push(to_u32(inst));
                continue;
            }
            let op = view.op(inst);
            if Ty(at(view.tys, inst)) != Ty::I64 {
                bad = true;
                break;
            }
            let ok = match op {
                Op::Iconst | Op::IconstWide | Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => true,
                Op::Shl | Op::Lshr => {
                    let idx = to_idx(at(view.data, inst));
                    const_u64(&view, to_idx(at(view.extra, idx + 1))).is_some_and(|k| k < 64)
                }
                _ => false,
            };
            if !ok {
                bad = true;
                break;
            }
            body_insts.push(to_u32(inst));
        }
        // Ensure no other instruction uses a `div_site.umulhi_inst` directly.
        for &bi in &body_insts {
            let bi_idx = to_idx(bi);
            if !at(&is_div_lshr, bi_idx) {
                each(&view, bi_idx, |item| {
                    if let Item::Value(v) = item
                        && at(&is_div_umulhi, to_idx(v))
                    {
                        bad = true;
                    }
                });
            }
        }
        if bad {
            continue;
        }

        let iv_init = at(&pre_args, iv_pos);
        found = Some(IvDivPlan {
            preheader,
            header,
            iv_pos,
            iv_inst,
            iv_init,
            bound,
            pre_args,
            reductions,
            div_sites,
            body_insts,
        });
        break;
    }

    let Some(plan) = found else {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    };

    // Check whether the body DAG is provably non-overflowing and non-borrowing in `u32` for
    // `iv < 2^30` (over 1 billion iterations!) with additive `u64` reductions. If so, vectorize
    // 4x `<4 x i32>` = 16 elements/iter using `Op::Umulhi` (`umull.2d` + `umull2.2d` + `uzp2.4s`)
    // and widening pairwise accumulate (`uadalp.2d`)!
    const U32_FAST_MAX: u64 = 1u64 << 30;
    let can_use_4xi32 = plan.reductions.iter().all(|r| r.op == Op::Add)
        && plan.div_sites.iter().all(|ds| ds.rshift >= 32 && ds.rshift < 64)
        && {
            // Track `[lo, hi]` value bounds for `iv in 0..=U32_FAST_MAX`, plus exact multiples
            // `Some((d, k))` meaning `val == k * (iv / d)` so `Sub(iv, k * (iv / d))` with `k <= d`
            // is proven non-negative (`>= 0`) without borrowing!
            let mut bounds: Vec<Option<(u64, u64)>, A> = Vec::with_capacity_in(len, alloc);
            let mut q_mul: Vec<Option<(u64, u64)>, A> = Vec::with_capacity_in(len, alloc);
            for _ in 0..len {
                bounds.push(None);
                q_mul.push(None);
            }
            set_at(&mut bounds, to_idx(plan.iv_inst), Some((0, U32_FAST_MAX)));
            let mut ok32 = true;
            for &bi in &plan.body_insts {
                let idx = to_idx(bi);
                if let Some(ds) = plan.div_sites.iter().find(|ds| ds.lshr_inst == bi) {
                    set_at(&mut bounds, idx, Some((0, U32_FAST_MAX / ds.divisor)));
                    set_at(&mut q_mul, idx, Some((ds.divisor, 1)));
                    continue;
                }
                let op = view.op(idx);
                let next_mask = |x: u64| if x == 0 { 0 } else { u64::MAX >> x.leading_zeros() };
                let bound_of = |v: usize, b: &[Option<(u64, u64)>]| -> Option<(u64, u64)> {
                    if let Some(bv) = at(b, v) {
                        return Some(bv);
                    }
                    const_u64(&view, v).filter(|&c| c <= u64::from(u32::MAX)).map(|c| (c, c))
                };
                match op {
                    Op::Iconst | Op::IconstWide => {
                        if let Some(c) = const_u64(&view, idx).filter(|&c| c <= u64::from(u32::MAX)) {
                            set_at(&mut bounds, idx, Some((c, c)));
                        } else {
                            ok32 = false;
                            break;
                        }
                    }
                    Op::Add => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), bound_of(bv, &bounds)) {
                            (Some((la, ha)), Some((lb, hb))) if ha.saturating_add(hb) <= u64::from(u32::MAX) => {
                                set_at(&mut bounds, idx, Some((la + lb, ha + hb)));
                                if let (Some((da, ka)), Some((db, kb))) = (at(&q_mul, a), at(&q_mul, bv))
                                    && da == db
                                {
                                    set_at(&mut q_mul, idx, ka.checked_add(kb).map(|k| (da, k)));
                                }
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::Sub => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        if a == to_idx(plan.iv_inst)
                            && let Some((d, k)) = at(&q_mul, bv)
                            && k <= d
                        {
                            set_at(&mut bounds, idx, Some((0, U32_FAST_MAX)));
                        } else if let (Some((la, ha)), Some((lb, hb))) = (bound_of(a, &bounds), bound_of(bv, &bounds))
                            && la >= hb
                        {
                            set_at(&mut bounds, idx, Some((la - hb, ha - lb)));
                        } else {
                            ok32 = false;
                            break;
                        }
                    }
                    Op::Shl => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), const_u64(&view, bv)) {
                            (Some((la, ha)), Some(k)) if k < 32 && (ha << k) <= u64::from(u32::MAX) => {
                                set_at(&mut bounds, idx, Some((la << k, ha << k)));
                                if let Some((da, ka)) = at(&q_mul, a) {
                                    let k32 = u32::try_from(k).unwrap_or(0);
                                    set_at(&mut q_mul, idx, ka.checked_shl(k32).map(|kk| (da, kk)));
                                }
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::Lshr => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), const_u64(&view, bv)) {
                            (Some((la, ha)), Some(k)) if k < 32 => {
                                set_at(&mut bounds, idx, Some((la >> k, ha >> k)));
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::And => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), bound_of(bv, &bounds)) {
                            (Some((_, ha)), Some((_, hb))) => set_at(&mut bounds, idx, Some((0, ha.min(hb)))),
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::Or | Op::Xor => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), bound_of(bv, &bounds)) {
                            (Some((_, ha)), Some((_, hb))) => {
                                let hi = next_mask(ha.max(hb));
                                if hi <= u64::from(u32::MAX) {
                                    set_at(&mut bounds, idx, Some((0, hi)));
                                } else {
                                    ok32 = false;
                                    break;
                                }
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    _ => {
                        ok32 = false;
                        break;
                    }
                }
            }
            if ok32 {
                for red in &plan.reductions {
                    if at(&bounds, to_idx(red.elem_val)).is_none() {
                        ok32 = false;
                        break;
                    }
                }
            }
            ok32
        };

    const UNROLL: usize = 4; // 4 x <4 x i32> = 16 iters/iter (fast) or 4 x <2 x i64> = 8 iters/iter!
    let (Some(vec_ty), Some(vec32_ty)) = (
        Ty::vector(crate::ty::Scalar::I64, 2),
        Ty::vector(crate::ty::Scalar::I32, 4),
    ) else {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    };
    let insert_at = to_idx(plan.preheader) + 1;
    let vpre_blk = to_u32(insert_at);
    let vloop_blk = to_u32(insert_at + 1);
    let vred_blk = to_u32(insert_at + 2);

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
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let bound_new = at(&val_map, to_idx(plan.bound));
            let step_total = if can_use_4xi32 { 16u64 } else { 8u64 };
            let bound_limit = if can_use_4xi32 { U32_FAST_MAX } else { u64::from(u32::MAX) };
            let has_rem = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
            let k_lim = push_iconst(&mut func, Ty::I64, bound_limit);
            let fits_u32 = push_icmp(&mut func, IntCond::Ule, bound_new, k_lim);
            let rem = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, bound_new, iv_init_new);
            let k_step = push_iconst(&mut func, Ty::I64, step_total);
            let enough = push_icmp(&mut func, IntCond::Uge, rem, k_step);
            let g0 = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, fits_u32);
            let go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, g0, enough);

            let mut fb_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for &a in &plan.pre_args {
                fb_args.push(at(&val_map, to_idx(a)));
            }
            push_br(&mut func, go_vec, vpre_blk, &[], orig_hdr_new, &fb_args);

            let body_blk_id = at(view.extra, to_idx(at(view.data, view.terminator(Block(plan.header)))) + 1);
            if can_use_4xi32 {
                // --- `vpre` (16-wide `<4 x i32>` path with `Umulhi` + `uadalp.2d`) ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let k_mask = push_iconst(&mut func, Ty::I64, !15u64);
                let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, k_mask);
                let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init_new, vcnt);

                let i0_32 = push_inst(&mut func, Op::Trunc, Flags::NONE, Ty::I32, iv_init_new, 0);
                let k1_32 = push_iconst(&mut func, Ty::I32, 1);
                let k2_32 = push_iconst(&mut func, Ty::I32, 2);
                let k3_32 = push_iconst(&mut func, Ty::I32, 3);
                let i1_32 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I32, i0_32, k1_32);
                let i2_32 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I32, i0_32, k2_32);
                let i3_32 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I32, i0_32, k3_32);
                let mut vi0_init = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, i0_32, 0);
                for (lane, iv_lane) in [(1u32, i1_32), (2, i2_32), (3, i3_32)] {
                    let ins_at = to_u32(func.extra.len());
                    func.extra.push(vi0_init);
                    func.extra.push(iv_lane);
                    func.extra.push(lane);
                    vi0_init = push_inst(&mut func, Op::Insert, Flags::NONE, vec32_ty, ins_at, 0);
                }

                let k4_32 = push_iconst(&mut func, Ty::I32, 4);
                let v_off4 = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, k4_32, 0);
                let k_mask32 = push_iconst(&mut func, Ty::I64, u64::from(u32::MAX));
                let v_mask32 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k_mask32, 0);
                let k_sh32 = push_iconst(&mut func, Ty::I64, 32);
                let v_sh32 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k_sh32, 0);

                let mut div_splats: Vec<(u32, u32, Option<u32>), A> =
                    Vec::with_capacity_in(plan.div_sites.len(), alloc);
                for ds in &plan.div_sites {
                    let km = push_iconst(&mut func, Ty::I32, u64::from(ds.magic32));
                    let vm = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, km, 0);
                    let rem_sh = ds.rshift - 32;
                    let vs = if rem_sh > 0 {
                        let ks = push_iconst(&mut func, Ty::I32, u64::from(rem_sh));
                        Some(push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, ks, 0))
                    } else {
                        None
                    };
                    div_splats.push((ds.lshr_inst, vm, vs));
                }

                let mut inv_splats: Vec<(u32, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    if plan.div_sites.iter().any(|ds| ds.lshr_inst == bi) {
                        continue;
                    }
                    each(&view, to_idx(bi), |item| {
                        if let Item::Value(v) = item {
                            let vi = to_idx(v);
                            let vb = at(&inst_block, vi);
                            if vb != plan.header && vb != body_blk_id && inv_splats.iter().all(|&(old, _)| old != v) {
                                let t32 = if let Some(c) = const_u64(&view, vi) {
                                    push_iconst(&mut func, Ty::I32, c)
                                } else {
                                    push_inst(&mut func, Op::Trunc, Flags::NONE, Ty::I32, at(&val_map, vi), 0)
                                };
                                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, t32, 0);
                                inv_splats.push((v, sp));
                            }
                        }
                    });
                }

                let mut vinit_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vinit_args.push(iv_init_new);
                vinit_args.push(vi0_init);
                let id_c = push_iconst(&mut func, Ty::I64, 0);
                for _ in &plan.reductions {
                    for _ in 0..UNROLL {
                        let vacc = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, id_c, 0);
                        vinit_args.push(vacc);
                    }
                }
                push_jump(&mut func, vloop_blk, &vinit_args);

                // --- `vloop`: 4 x `<4 x i32>` = 16 elements/iter via `Umulhi` + `uadalp.2d`! ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_v = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let vi0 = push_inst(&mut func, Op::Param, Flags::NONE, vec32_ty, 0, 0);
                let mut vacc_params: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    let a0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    vacc_params.push([a0, a1, a2, a3]);
                }

                let mut body_const_splats: Vec<(usize, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    let b_idx = to_idx(bi);
                    if matches!(view.op(b_idx), Op::Iconst | Op::IconstWide)
                        && let Some(cval) = const_u64(&view, b_idx)
                    {
                        let c32 = push_iconst(&mut func, Ty::I32, cval);
                        let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, c32, 0);
                        body_const_splats.push((b_idx, sp));
                    }
                }

                let mut next_accs: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    next_accs.push([NONE; UNROLL]);
                }
                let mut vmap = filled(len, NONE, &alloc);
                let mut cur_vi = vi0;
                for u in 0..UNROLL {
                    let vi_u = if u == 0 {
                        cur_vi
                    } else {
                        cur_vi = push_bin(&mut func, Op::Add, Flags::NONE, vec32_ty, cur_vi, v_off4);
                        cur_vi
                    };
                    set_at(&mut vmap, to_idx(plan.iv_inst), vi_u);
                    for &(old, sp) in &inv_splats {
                        set_at(&mut vmap, to_idx(old), sp);
                    }
                    for &(b_idx, sp) in &body_const_splats {
                        set_at(&mut vmap, b_idx, sp);
                    }
                    for &bi in &plan.body_insts {
                        let b_idx = to_idx(bi);
                        if let Some(&(_, vm32, vsh_opt)) = div_splats.iter().find(|&&(li, _, _)| li == bi) {
                            let hi = push_bin(&mut func, Op::Umulhi, Flags::NONE, vec32_ty, vi_u, vm32);
                            let res = if let Some(vsh) = vsh_opt {
                                push_bin(&mut func, Op::Lshr, Flags::NONE, vec32_ty, hi, vsh)
                            } else {
                                hi
                            };
                            set_at(&mut vmap, b_idx, res);
                        } else {
                            let op = view.op(b_idx);
                            if !matches!(op, Op::Iconst | Op::IconstWide) {
                                let idx = to_idx(at(view.data, b_idx));
                                let (a, bv) = (to_idx(at(view.extra, idx)), to_idx(at(view.extra, idx + 1)));
                                let nv = push_bin(&mut func, op, Flags::NONE, vec32_ty, at(&vmap, a), at(&vmap, bv));
                                set_at(&mut vmap, b_idx, nv);
                            }
                        }
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        // Canonical `uadalp.2d` pattern: zero-extend & pairwise add `<4 x i32>` into `<2 x i64>`!
                        let e_vec = at(&vmap, to_idx(red.elem_val));
                        let bc = push_inst(&mut func, Op::Bitcast, Flags::NONE, vec_ty, e_vec, 0);
                        let lo = push_bin(&mut func, Op::And, Flags::NONE, vec_ty, bc, v_mask32);
                        let hi = push_bin(&mut func, Op::Lshr, Flags::NONE, vec_ty, bc, v_sh32);
                        let acc_u = vacc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                        let tmp = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, acc_u, lo);
                        let nv = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, tmp, hi);
                        if let Some(row) = next_accs.get_mut(ridx) {
                            set_at(row, u, nv);
                        }
                    }
                }

                let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_v, k_step);
                let vi0_next = push_bin(&mut func, Op::Add, Flags::NONE, vec32_ty, cur_vi, v_off4);
                let mut vback_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vback_args.push(iv_next);
                vback_args.push(vi0_next);
                for row in &next_accs {
                    for &v in row {
                        vback_args.push(v);
                    }
                }
                let keep = push_icmp(&mut func, IntCond::Ult, iv_next, vend);
                push_br(&mut func, keep, vloop_blk, &vback_args, vred_blk, &vback_args);

                // --- `vred`: tree-reduce 4 `<2 x i64>` accumulators & jump to scalar tail ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_done = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let _vi_done = push_inst(&mut func, Op::Param, Flags::NONE, vec32_ty, 0, 0);
                let mut red_scalars: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for red in &plan.reductions {
                    let d0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let c01 = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, d0, d1);
                    let c23 = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, d2, d3);
                    let comb = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, c01, c23);
                    let r_lane = push_inst(&mut func, Op::ReduceAdd, Flags::NONE, Ty::I64, comb, 0);
                    red_scalars.push(push_bin(
                        &mut func,
                        Op::Add,
                        Flags::NONE,
                        Ty::I64,
                        at(&val_map, to_idx(red.init_val)),
                        r_lane,
                    ));
                }
                let mut tail_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
                for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                    if pos == plan.iv_pos {
                        tail_args.push(iv_done);
                    } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                        tail_args.push(at(&red_scalars, ridx));
                    } else {
                        tail_args.push(at(&val_map, to_idx(pre_arg)));
                    }
                }
                push_jump(&mut func, orig_hdr_new, &tail_args);
            } else {
                // --- `vpre`: build initial `<2 x i64>` `[i0, i0+1]`, splat magic32 constants & masks ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let k_mask = push_iconst(&mut func, Ty::I64, !7u64);
                let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, k_mask);
                let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init_new, vcnt);

                let k1 = push_iconst(&mut func, Ty::I64, 1);
                let iv_p1 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init_new, k1);
                let v_i0_s = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, iv_init_new, 0);
                let ins_at = to_u32(func.extra.len());
                func.extra.push(v_i0_s);
                func.extra.push(iv_p1);
                func.extra.push(1);
                let vi0_init = push_inst(&mut func, Op::Insert, Flags::NONE, vec_ty, ins_at, 0);

                let k2 = push_iconst(&mut func, Ty::I64, 2);
                let v_off2 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k2, 0);
                let v_mask32 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k_lim, 0);

                let mut div_splats: Vec<(u32, u32, u32), A> = Vec::with_capacity_in(plan.div_sites.len(), alloc);
                for ds in &plan.div_sites {
                    let km = push_iconst(&mut func, Ty::I64, u64::from(ds.magic32));
                    let vm = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, km, 0);
                    let ks = push_iconst(&mut func, Ty::I64, u64::from(ds.rshift));
                    let vs = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, ks, 0);
                    div_splats.push((ds.lshr_inst, vm, vs));
                }
                // Hoist any constant shifts / loop-invariant scalars used by `body_insts`.
                let mut inv_splats: Vec<(u32, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    if plan.div_sites.iter().any(|ds| ds.lshr_inst == bi) {
                        continue;
                    }
                    each(&view, to_idx(bi), |item| {
                        if let Item::Value(v) = item {
                            let vi = to_idx(v);
                            let vb = at(&inst_block, vi);
                            if vb != plan.header && vb != body_blk_id && inv_splats.iter().all(|&(old, _)| old != v) {
                                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, at(&val_map, vi), 0);
                                inv_splats.push((v, sp));
                            }
                        }
                    });
                }

                let mut vinit_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vinit_args.push(iv_init_new);
                vinit_args.push(vi0_init);
                for red in &plan.reductions {
                    let id_c = push_iconst(&mut func, Ty::I64, if red.op == Op::And { u64::MAX } else { 0 });
                    for _ in 0..UNROLL {
                        let vacc = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, id_c, 0);
                        vinit_args.push(vacc);
                    }
                }
                push_jump(&mut func, vloop_blk, &vinit_args);

                // --- `vloop`: 4x `<2 x i64>` = 8 iterations/iter with `umull.2d` + `ushr.2d` (10 regs, 0 spills)! ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_v = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let vi0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                let mut vacc_params: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    let a0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    vacc_params.push([a0, a1, a2, a3]);
                }

                // Hoist any loop-body constants once at the top of `vloop` (constant splat).
                let mut body_const_splats: Vec<(usize, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    let b_idx = to_idx(bi);
                    if matches!(view.op(b_idx), Op::Iconst | Op::IconstWide) {
                        let c_new = copy_inst_remapped_blocks(&view, b_idx, &val_map, &blk_map, &mut func);
                        let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, c_new, 0);
                        body_const_splats.push((b_idx, sp));
                    }
                }

                // Interleave lane-by-lane (`u = 0..4`) chaining `vi_{u+1} = vi_u + v_off2` so peak
                // live vector register count stays at ~10 (ZERO stack spills on AArch64!).
                let mut next_accs: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    next_accs.push([NONE; UNROLL]);
                }
                let mut vmap = filled(len, NONE, &alloc);
                let mut cur_vi = vi0;
                for u in 0..UNROLL {
                    let vi_u = if u == 0 {
                        cur_vi
                    } else {
                        cur_vi = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, cur_vi, v_off2);
                        cur_vi
                    };
                    set_at(&mut vmap, to_idx(plan.iv_inst), vi_u);
                    for &(old, sp) in &inv_splats {
                        set_at(&mut vmap, to_idx(old), sp);
                    }
                    for &(b_idx, sp) in &body_const_splats {
                        set_at(&mut vmap, b_idx, sp);
                    }
                    for &bi in &plan.body_insts {
                        let b_idx = to_idx(bi);
                        if let Some(&(_, vm32, vsh)) = div_splats.iter().find(|&&(li, _, _)| li == bi) {
                            let lo32 = push_bin(&mut func, Op::And, Flags::NONE, vec_ty, vi_u, v_mask32);
                            let prod = push_bin(&mut func, Op::Mul, Flags::NONE, vec_ty, lo32, vm32);
                            let res = push_bin(&mut func, Op::Lshr, Flags::NONE, vec_ty, prod, vsh);
                            set_at(&mut vmap, b_idx, res);
                        } else {
                            let op = view.op(b_idx);
                            if !matches!(op, Op::Iconst | Op::IconstWide) {
                                let idx = to_idx(at(view.data, b_idx));
                                let (a, bv) = (to_idx(at(view.extra, idx)), to_idx(at(view.extra, idx + 1)));
                                let nv = push_bin(&mut func, op, Flags::NONE, vec_ty, at(&vmap, a), at(&vmap, bv));
                                set_at(&mut vmap, b_idx, nv);
                            }
                        }
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        let acc_u = vacc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                        let nv = push_bin(
                            &mut func,
                            red.op,
                            Flags::NONE,
                            vec_ty,
                            acc_u,
                            at(&vmap, to_idx(red.elem_val)),
                        );
                        if let Some(row) = next_accs.get_mut(ridx) {
                            set_at(row, u, nv);
                        }
                    }
                }

                let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_v, k_step);
                let vi0_next = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, cur_vi, v_off2);
                let mut vback_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vback_args.push(iv_next);
                vback_args.push(vi0_next);
                for row in &next_accs {
                    for &v in row {
                        vback_args.push(v);
                    }
                }
                let keep = push_icmp(&mut func, IntCond::Ult, iv_next, vend);
                push_br(&mut func, keep, vloop_blk, &vback_args, vred_blk, &vback_args);

                // --- `vred`: tree-reduce 4 `<2 x i64>` accumulators & jump to scalar tail ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_done = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let _vi_done = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                let mut red_scalars: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for red in &plan.reductions {
                    let d0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let c01 = push_bin(&mut func, red.op, Flags::NONE, vec_ty, d0, d1);
                    let c23 = push_bin(&mut func, red.op, Flags::NONE, vec_ty, d2, d3);
                    let comb = push_bin(&mut func, red.op, Flags::NONE, vec_ty, c01, c23);
                    let rop = match red.op {
                        Op::Add => Op::ReduceAdd,
                        Op::Xor => Op::ReduceXor,
                        Op::Or => Op::ReduceOr,
                        Op::And => Op::ReduceAnd,
                        _ => {
                            return Err(SimplifyError::Refused {
                                inst: to_idx(red.update_inst),
                                what: "unsupported vector reduction op",
                            });
                        }
                    };
                    let r_lane = push_inst(&mut func, rop, Flags::NONE, Ty::I64, comb, 0);
                    red_scalars.push(push_bin(
                        &mut func,
                        red.op,
                        Flags::NONE,
                        Ty::I64,
                        at(&val_map, to_idx(red.init_val)),
                        r_lane,
                    ));
                }
                let mut tail_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
                for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                    if pos == plan.iv_pos {
                        tail_args.push(iv_done);
                    } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                        tail_args.push(at(&red_scalars, ridx));
                    } else {
                        tail_args.push(at(&val_map, to_idx(pre_arg)));
                    }
                }
                push_jump(&mut func, orig_hdr_new, &tail_args);
            }
        } else {
            for inst in range {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(VectorizeOutput {
        func,
        stats: VectorizeStats { loops_vectorized: 1 },
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
    fn auto_vectorizes_u64_sum_loop_into_2x_unrolled_simd_and_verifies() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
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
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

        b.begin(body);
        let k3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, i, k3);
        let ep = b.ptr_add(Flags::NONE, ptr, off);
        let v = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, v);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        let has_reduce_add = (0..v.len()).any(|i| v.op(i) == Op::ReduceAdd);
        let has_vec_load = (0..v.len()).any(|i| v.op(i) == Op::Load && Ty(v.tys[i]).is_vector());
        assert!(has_reduce_add && has_vec_load, "expected 128-bit vector loads and ReduceAdd");
    }

    #[test]
    fn auto_vectorizes_u32_map_store_loop_with_alias_guard_and_verifies() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let key = b.param(Ty::I32);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

        b.begin(body);
        let k2 = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, k2);
        let sp = b.ptr_add(Flags::NONE, src, off);
        let dp = b.ptr_add(Flags::NONE, dst, off);
        let val = b.load(Ty::I32, Flags::NONE, sp, 0, MemAttrs::aligned(4));
        let xored = b.binary(Op::Xor, Flags::NONE, val, key);
        b.store(Flags::NONE, xored, dp, 0, MemAttrs::aligned(4));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        let vec_stores = (0..v.len())
            .filter(|&i| v.op(i) == Op::Store && Ty(v.tys[v.extra[v.data[i] as usize] as usize]).is_vector())
            .count();
        assert_eq!(vec_stores, 4, "expected 4x-unrolled adjacent 128-bit vector stores (64B/iter)");
    }

    #[test]
    fn vectorize_iv_div_loops_16wide_uadalp_matches_evaluator() {
        use crate::{
            egraph::{RegBudget, sota_optimize_fixpoint},
            eval::{self, Outcome, Val},
        };
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((header, &[z, z]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let c10 = b.iconst(Ty::I64, 10);
        let c1 = b.iconst(Ty::I64, 1);
        let q = b.binary(Op::Udiv, Flags::NONE, i, c10);
        let r = b.binary(Op::Urem, Flags::NONE, i, c10);
        let t = b.binary(Op::Add, Flags::NONE, q, r);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, t);
        let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let opt = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 4, |_| 1, TestHeap).unwrap();
        let vec = vectorize_iv_div_loops(opt.func.view(), TestHeap).unwrap();
        assert_eq!(vec.stats.loops_vectorized, 1);
        let v = vec.func.view();
        assert!(
            (0..v.len()).any(|idx| v.op(idx) == Op::Umulhi && Ty(v.tys[idx]).is_vector()),
            "expected `<4 x i32>` vector `Op::Umulhi` in vectorized div10 loop"
        );

        for &nv in &[0u128, 1, 7, 8, 15, 16, 17, 31, 32, 33, 100] {
            let Outcome::Returned(orig) = eval::run_in(func.view(), &[Val::Int(nv)], 20_000, TestHeap).unwrap() else {
                panic!("orig eval");
            };
            let Outcome::Returned(got) = eval::run_in(vec.func.view(), &[Val::Int(nv)], 20_000, TestHeap).unwrap() else {
                panic!("vec eval");
            };
            assert_eq!(orig, got, "vectorize_iv_div_loops mismatch at n={nv}");
        }
    }
}

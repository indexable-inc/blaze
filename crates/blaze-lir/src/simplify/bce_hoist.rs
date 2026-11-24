//! Safe-Rust Loop-Versioning Bounds-Check Hoister (`bce_hoist`).
//!
//! Real safe Rust slice-indexing loops (`a[i]`, `a[i + c]`, `a[i * n + k]`, `b[k * n + j]`,
//! `c[i * n + j]`) contain per-iteration bounds-check branches whose cold targets terminate in
//! `Op::Trap` or `Op::Unreachable` (optionally after a `#[cold]`/`#[noreturn]` panic call).
//! Those in-loop trap branches fragment loop bodies into multi-block CFGs that prevent
//! [`super::auto_vectorize_loops`], [`super::optimize_gemm_loops`], [`super::lower_simd_idioms`],
//! and [`super::unroll_scalar_loops`] from firing.
//!
//! [`hoist_loop_bounds_checks`] discovers counted 1D/2D/3D natural loops, iteratively classifies
//! affine bounds-check indices (bounded depth, zero recursion), synthesizes overflow-free
//! loop-invariant preheader guards, and versions each eligible loop (or outermost loop nest):
//!
//! - **Fast clone (`BlockKind::Normal`)**: every hoisted bounds-check `Op::Br` is replaced by an
//!   unconditional `Op::Jump` straight to the in-bounds continuation, and `dead` + `coalesce_cfg`
//!   collapse the fast body back into clean single-block loops behind a dedicated `Op::Jump`
//!   preheader — immediately unlocking NEON auto-vectorization and Auto-SME2 GEMM lowering.
//! - **Slow fallback (`BlockKind::Cold`)**: the untouched original loop executes whenever any
//!   hoisted guard fails, preserving exact Rust out-of-bounds panic semantics on the exact
//!   offending iteration and ensuring strict idempotence on repeated runs.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, `#![no_std]` compatible, deterministic (Rule 25),
//! bounded loops (Rule 18), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, coalesce_cfg, dead, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    op::{Field, Layout},
    verify,
};

/// Maximum number of loops versioned in a single call to [`hoist_loop_bounds_checks`].
const MAX_LOOPS_PER_FUNC: usize = 16;

/// Maximum depth for iterative affine index matching (zero recursion).
const MAX_AFFINE_DEPTH: u32 = 8;

/// Maximum single-successor `Op::Jump` / `Op::Invoke` hops when recognizing a panic sink block.
const MAX_SINK_HOPS: usize = 8;

/// Configuration for [`hoist_loop_bounds_checks`] (Rule 83: single options struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BceHoistOpts {
    /// Maximum number of basic blocks inside a candidate loop (or nest) to version.
    pub max_loop_blocks: usize,
    /// Maximum total instructions inside a candidate loop (or nest) to version.
    pub max_loop_insts: usize,
    /// Maximum number of hoisted bounds-check branches per versioned loop.
    pub max_checks: usize,
}

impl BceHoistOpts {
    /// SOTA default budgets for 1D vectorizable loops and 2D/3D matrix nests.
    pub const fn default_sota() -> Self {
        Self {
            max_loop_blocks: 64,
            max_loop_insts: 512,
            max_checks: 16,
        }
    }
}

impl Default for BceHoistOpts {
    fn default() -> Self {
        Self::default_sota()
    }
}

/// Telemetry returned by [`hoist_loop_bounds_checks`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct BceHoistStats {
    /// Number of loops (or loop nests) versioned with hoisted bounds-check guards.
    pub loops_versioned: u32,
    /// Total number of in-loop bounds-check branches folded out of fast clones.
    pub checks_hoisted: u32,
}

/// Output of [`hoist_loop_bounds_checks`].
#[derive(Debug)]
pub struct BceHoistOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: BceHoistStats,
}

// ---------------------------------------------------------------------------
// Low-level ratchet-clean indexing, conversion, and encoding helpers
// ---------------------------------------------------------------------------

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    let [b0, b1, b2, b3, ..] = v.to_le_bytes();
    u32::from_le_bytes([b0, b1, b2, b3])
}

#[inline]
fn trunc_u64(v: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = v.to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
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
    Op::ALL
        .iter()
        .position(|&o| o == op)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0)
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
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() || ty.bits(view.head.ptr_bits) > 64 {
        return None;
    }
    if at(view.flags, inst) != 0 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, width),
        Op::IconstWide => {
            let start = to_idx(view.data.get(inst).copied()?);
            let end = start.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(start..end)?, width)
        }
        _ => return None,
    };
    Some(trunc_u64(v))
}

#[inline]
fn same_val_or_const(view: &View<'_>, a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    let (ai, bi) = (to_idx(a), to_idx(b));
    at(view.tys, ai) == at(view.tys, bi)
        && const_u64(view, ai).is_some()
        && const_u64(view, ai) == const_u64(view, bi)
}

#[inline]
fn fits_in_ty(c: u64, ty: Ty) -> bool {
    if ty == Ty::I64 {
        true
    } else if ty == Ty::I32 {
        c <= u64::from(u32::MAX)
    } else {
        false
    }
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

fn push_inst<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    data: u32,
    attrs: u32,
    loc: u32,
) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.bits());
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attrs);
    func.locs.push(loc);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64, loc: u32) -> u32 {
    let bits = ty.bits(func.head.ptr_bits);
    if bits < 32 {
        let masked = trunc_u32(val & ((1u64 << bits) - 1));
        push_inst(func, Op::Iconst, Flags::NONE, ty, masked, 0, loc)
    } else if bits == 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val), 0, loc)
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned(), 0, loc)
    } else {
        let pos = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, pos, 0, loc)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, ty: Ty, a: u32, b: u32, loc: u32) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, Flags::NONE, ty, pos, 0, loc)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32, loc: u32) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, pos, 0, loc)
}

fn push_br<A: Allocator>(
    func: &mut Func<A>,
    cond: u32,
    t_blk: u32,
    t_args: &[u32],
    e_blk: u32,
    e_args: &[u32],
    loc: u32,
) -> u32 {
    let pos = to_u32(func.extra.len());
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
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, pos, 0, loc)
}

fn copy_u32_vec<A: Allocator + Copy>(src: &[u32], alloc: A) -> Vec<u32, A> {
    let mut dst = Vec::with_capacity_in(src.len(), alloc);
    for &v in src {
        dst.push(v);
    }
    dst
}

// ---------------------------------------------------------------------------
// CFG, Dominators, & Panic-Sink Detection
// ---------------------------------------------------------------------------

struct CfgInfo<A: Allocator> {
    inst_block: Vec<u32, A>,
    preds: Vec<Vec<u32, A>, A>,
    idom: Vec<u32, A>,
}

impl<A: Allocator + Copy> CfgInfo<A> {
    fn build(view: &View<'_>, alloc: A) -> Self {
        let blocks = view.blocks();
        let len = view.len();
        let mut inst_block = filled(len, 0u32, &alloc);
        let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
        let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
        for _ in 0..blocks {
            preds.push(Vec::new_in(alloc));
            succs.push(Vec::new_in(alloc));
        }

        for b in 0..blocks {
            let b32 = to_u32(b);
            for inst in view.block(Block(b32)) {
                set_at(&mut inst_block, inst, b32);
            }
            let term = view.terminator(Block(b32));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let tb = to_idx(edge.block);
                    if let Some(st) = succs.get_mut(b)
                        && st.scan_for(&edge.block).is_none()
                    {
                        st.push(edge.block);
                    }
                    if let Some(pt) = preds.get_mut(tb)
                        && pt.scan_for(&b32).is_none()
                    {
                        pt.push(b32);
                    }
                }
            });
        }

        // Iterative reverse postorder DFS from entry block 0.
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut seen = filled(blocks, false, &alloc);
        let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, 0));
        }
        let max_dfs = blocks.saturating_mul(8).saturating_add(16);
        for _ in 0..max_dfs {
            let Some(&(b, next_idx)) = stack.last() else {
                break;
            };
            let maybe_succ = succs.get(to_idx(b)).and_then(|s| s.get(next_idx).copied());
            if let Some(s) = maybe_succ {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                if !at(&seen, to_idx(s)) {
                    set_at(&mut seen, to_idx(s), true);
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
                let _ = stack.pop();
            }
        }

        let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), alloc);
        let mut order = filled(blocks, NONE, &alloc);
        for (i, &b) in post.iter().rev().enumerate() {
            rpo.push(b);
            set_at(&mut order, to_idx(b), to_u32(i));
        }

        // Cooper-Harvey-Kennedy iterative dominators.
        let mut idom = filled(blocks, NONE, &alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        let max_rounds = blocks.saturating_mul(2).saturating_add(2);
        for _ in 0..max_rounds {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new_idom = NONE;
                if let Some(p_list) = preds.get(to_idx(b)) {
                    for &p in p_list {
                        if at(&idom, to_idx(p)) == NONE {
                            continue;
                        }
                        new_idom = if new_idom == NONE {
                            p
                        } else {
                            dom_intersect(&idom, &order, p, new_idom)
                        };
                    }
                }
                if new_idom != at(&idom, to_idx(b)) {
                    set_at(&mut idom, to_idx(b), new_idom);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        Self { inst_block, preds, idom }
    }

    fn dominates(&self, a: u32, mut b: u32) -> bool {
        if at(&self.idom, to_idx(a)) == NONE || at(&self.idom, to_idx(b)) == NONE {
            return false;
        }
        for _ in 0..self.idom.len() {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = at(&self.idom, to_idx(b));
        }
        false
    }
}

fn dom_intersect(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    let bound = idom.len().saturating_mul(2).saturating_add(2);
    for _ in 0..bound {
        if a == b {
            return a;
        }
        if at(order, to_idx(a)) > at(order, to_idx(b)) {
            a = at(idom, to_idx(a));
        } else {
            b = at(idom, to_idx(b));
        }
    }
    a
}

/// Returns `true` if block `b` is a cold panic/trap sink outside `in_loop`:
///
/// - `b` is NOT in `in_loop` (cannot reach `header`), and
/// - `b` terminates directly in `Op::Trap` or `Op::Unreachable` (optionally after a `#[cold]` /
///   `#[noreturn]` panic `Op::Call`), OR terminates in an unwinding panic `Op::Invoke` /
///   `Op::InvokeIndirect` whose normal continuation leads to `Op::Trap`/`Op::Unreachable` and whose
///   unwind pad is outside `in_loop`, OR follows a short chain (<= `MAX_SINK_HOPS`) of non-loop
///   `Op::Jump` trampolines to a block terminating in `Op::Trap` or `Op::Unreachable`.
fn is_panic_sink_block(view: &View<'_>, mut b: u32, in_loop: &[bool]) -> bool {
    let mut visited = [NONE; MAX_SINK_HOPS];
    for hop in 0..MAX_SINK_HOPS {
        let bi = to_idx(b);
        if bi >= view.blocks() || at(in_loop, bi) {
            return false;
        }
        if visited.iter().take(hop).any(|&v| v == b) {
            return false;
        }
        set_at(&mut visited, hop, b);
        let term = view.terminator(Block(b));
        match view.op(term) {
            Op::Trap | Op::Unreachable => return true,
            Op::Jump => {
                let pos = to_idx(at(view.data, term));
                b = at(view.extra, pos);
            }
            Op::Invoke | Op::InvokeIndirect => {
                // Layout: `[Ref|Value, Sig, Block(normal), Block(unwind), Values]`.
                let pos = to_idx(at(view.data, term));
                let normal_blk = at(view.extra, pos + 2);
                let unwind_blk = at(view.extra, pos + 3);
                if to_idx(unwind_blk) >= view.blocks() || at(in_loop, to_idx(unwind_blk)) {
                    return false;
                }
                b = normal_blk;
            }
            _ => return false,
        }
    }
    false
}

/// Unwraps up to 6 layers of boolean wrappers (`Op::Freeze`, `Op::Not`, `Op::Xor(_, 1)`,
/// `Op::Trunc(Op::Zext(i1))`, or `Op::Icmp(Eq|Ne, bool_or_zext_bool, 0|1)`) around an `i1`
/// condition, returning `(inner_cond, flipped)`.
fn unwrap_bool_cond(view: &View<'_>, mut cond_val: u32) -> (u32, bool) {
    let mut flipped = false;
    for _ in 0..6 {
        let ci = to_idx(cond_val);
        if ci >= view.len() {
            break;
        }
        match view.op(ci) {
            Op::Freeze => {
                cond_val = at(view.data, ci);
            }
            Op::Not => {
                cond_val = at(view.data, ci);
                flipped = !flipped;
            }
            Op::Trunc => {
                let src = at(view.data, ci);
                let si = to_idx(src);
                if si < view.len() && view.op(si) == Op::Zext {
                    let inner = at(view.data, si);
                    if Ty(at(view.tys, to_idx(inner))) == Ty::I1 {
                        cond_val = inner;
                        continue;
                    }
                }
                break;
            }
            Op::Xor => {
                let pos = to_idx(at(view.data, ci));
                let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
                if const_u64(view, to_idx(r)) == Some(1) {
                    cond_val = l;
                    flipped = !flipped;
                } else if const_u64(view, to_idx(l)) == Some(1) {
                    cond_val = r;
                    flipped = !flipped;
                } else {
                    break;
                }
            }
            Op::Icmp => {
                let cat = to_idx(at(view.data, ci));
                let Some(ic) = IntCond::from_word(at(view.extra, cat)) else {
                    break;
                };
                if ic != IntCond::Eq && ic != IntCond::Ne {
                    break;
                }
                let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
                let (val, c) = if let Some(k) = const_u64(view, to_idx(rhs)).filter(|&k| k <= 1) {
                    (lhs, k)
                } else if let Some(k) = const_u64(view, to_idx(lhs)).filter(|&k| k <= 1) {
                    (rhs, k)
                } else {
                    break;
                };
                let vi = to_idx(val);
                let inner_i1 = if Ty(at(view.tys, vi)) == Ty::I1 {
                    val
                } else if view.op(vi) == Op::Zext {
                    let src = at(view.data, vi);
                    if Ty(at(view.tys, to_idx(src))) == Ty::I1 {
                        src
                    } else {
                        break;
                    }
                } else {
                    break;
                };
                let is_inverted = (c == 0 && ic == IntCond::Eq) || (c == 1 && ic == IntCond::Ne);
                cond_val = inner_i1;
                if is_inverted {
                    flipped = !flipped;
                }
            }
            _ => break,
        }
    }
    (cond_val, flipped)
}

// ---------------------------------------------------------------------------
// Counted Loop Discovery (`0..n` or `start..end`, `Ult`/`Slt`, step `+1`)
// ---------------------------------------------------------------------------

struct BrInfo<A: Allocator> {
    cond: u32,
    t_blk: u32,
    t_args: Vec<u32, A>,
    e_blk: u32,
    e_args: Vec<u32, A>,
}

fn read_br<A: Allocator + Copy>(view: &View<'_>, term: usize, alloc: A) -> Option<BrInfo<A>> {
    if view.op(term) != Op::Br {
        return None;
    }
    let pos = to_idx(at(view.data, term));
    let cond = at(view.extra, pos);
    let t_blk = at(view.extra, pos + 1);
    let t_argc = to_idx(at(view.extra, pos + 2));
    let mut t_args = Vec::with_capacity_in(t_argc, alloc);
    for k in 0..t_argc {
        t_args.push(at(view.extra, pos + 3 + k));
    }
    let e_at = pos + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));
    let mut e_args = Vec::with_capacity_in(e_argc, alloc);
    for k in 0..e_argc {
        e_args.push(at(view.extra, e_at + 2 + k));
    }
    Some(BrInfo { cond, t_blk, t_args, e_blk, e_args })
}

fn read_jump<A: Allocator + Copy>(view: &View<'_>, term: usize, alloc: A) -> Option<(u32, Vec<u32, A>)> {
    if view.op(term) != Op::Jump {
        return None;
    }
    let pos = to_idx(at(view.data, term));
    let target = at(view.extra, pos);
    let argc = to_idx(at(view.extra, pos + 1));
    let mut args = Vec::with_capacity_in(argc, alloc);
    for k in 0..argc {
        args.push(at(view.extra, pos + 2 + k));
    }
    Some((target, args))
}

#[inline]
fn match_add_operands(view: &View<'_>, inst: u32) -> Option<(u32, u32)> {
    let i = to_idx(inst);
    if i >= view.len() {
        return None;
    }
    match view.op(i) {
        Op::Add => {
            let pos = to_idx(at(view.data, i));
            Some((at(view.extra, pos), at(view.extra, pos + 1)))
        }
        Op::Project => {
            let pos = to_idx(at(view.data, i));
            let pair = at(view.extra, pos);
            let field = at(view.extra, pos + 1);
            let pi = to_idx(pair);
            if field == 0 && pi < view.len() && view.op(pi) == Op::UaddOverflow {
                let apos = to_idx(at(view.data, pi));
                Some((at(view.extra, apos), at(view.extra, apos + 1)))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[inline]
fn is_step_one(view: &View<'_>, inst: u32, iv: u32) -> bool {
    let Some((a, b)) = match_add_operands(view, inst) else {
        return false;
    };
    (a == iv && const_u64(view, to_idx(b)) == Some(1))
        || (b == iv && const_u64(view, to_idx(a)) == Some(1))
}

#[inline]
fn step_stride_of(view: &View<'_>, inst: u32, param: u32) -> Option<u32> {
    let (a, b) = match_add_operands(view, inst)?;
    if a == param {
        Some(b)
    } else if b == param {
        Some(a)
    } else {
        None
    }
}

/// Parses `cond` testing `iv < bound` (true -> stay, false -> exit) or swapped/inverted equivalents
/// (including `iv != bound` / `!(iv == bound)` when `start_is_zero`).
/// Returns `(bound, is_signed)`.
fn parse_lt_cond(
    view: &View<'_>,
    cond_val: u32,
    iv_val: u32,
    start_is_zero: bool,
    stay_is_then: bool,
) -> Option<(u32, bool)> {
    let (inner_cond, flipped) = unwrap_bool_cond(view, cond_val);
    let stay = stay_is_then ^ flipped;
    let ci = to_idx(inner_cond);
    if view.op(ci) != Op::Icmp {
        return None;
    }
    let cat = to_idx(at(view.data, ci));
    let cond = IntCond::from_word(at(view.extra, cat))?;
    let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
    if stay {
        if lhs == iv_val && cond == IntCond::Ult {
            Some((rhs, false))
        } else if lhs == iv_val && cond == IntCond::Slt {
            Some((rhs, true))
        } else if rhs == iv_val && cond == IntCond::Ugt {
            Some((lhs, false))
        } else if rhs == iv_val && cond == IntCond::Sgt {
            Some((lhs, true))
        } else if start_is_zero && cond == IntCond::Ne && lhs == iv_val {
            Some((rhs, false))
        } else if start_is_zero && cond == IntCond::Ne && rhs == iv_val {
            Some((lhs, false))
        } else {
            None
        }
    } else if lhs == iv_val && cond == IntCond::Uge {
        Some((rhs, false))
    } else if lhs == iv_val && cond == IntCond::Sge {
        Some((rhs, true))
    } else if rhs == iv_val && cond == IntCond::Ule {
        Some((lhs, false))
    } else if rhs == iv_val && cond == IntCond::Sle {
        Some((lhs, true))
    } else if start_is_zero && cond == IntCond::Eq && lhs == iv_val {
        Some((rhs, false))
    } else if start_is_zero && cond == IntCond::Eq && rhs == iv_val {
        Some((lhs, false))
    } else {
        None
    }
}

/// Checks whether a rotated preheader branch guards `start < bound`.
fn preheader_guards_entry(
    view: &View<'_>,
    cond_val: u32,
    start: u32,
    bound: u32,
    is_signed: bool,
    enter_is_then: bool,
) -> bool {
    let (inner_cond, flipped) = unwrap_bool_cond(view, cond_val);
    let enter = enter_is_then ^ flipped;
    let ci = to_idx(inner_cond);
    if view.op(ci) != Op::Icmp {
        return false;
    }
    let cat = to_idx(at(view.data, ci));
    let Some(cond) = IntCond::from_word(at(view.extra, cat)) else {
        return false;
    };
    let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
    let start_is_zero = const_u64(view, to_idx(start)) == Some(0);
    let lhs_start = same_val_or_const(view, lhs, start);
    let rhs_start = same_val_or_const(view, rhs, start);
    let lhs_bound = same_val_or_const(view, lhs, bound);
    let rhs_bound = same_val_or_const(view, rhs, bound);

    if enter {
        if is_signed {
            (lhs_start && rhs_bound && cond == IntCond::Slt)
                || (lhs_bound && rhs_start && cond == IntCond::Sgt)
        } else {
            (lhs_start && rhs_bound && (cond == IntCond::Ult || (start_is_zero && cond == IntCond::Ne)))
                || (lhs_bound && rhs_start && (cond == IntCond::Ugt || (start_is_zero && cond == IntCond::Ne)))
        }
    } else if is_signed {
        (lhs_start && rhs_bound && cond == IntCond::Sge)
            || (lhs_bound && rhs_start && cond == IntCond::Sle)
    } else {
        (lhs_start && rhs_bound && (cond == IntCond::Uge || (start_is_zero && cond == IntCond::Eq)))
            || (lhs_bound && rhs_start && (cond == IntCond::Ule || (start_is_zero && cond == IntCond::Eq)))
    }
}

struct CountedLoop<A: Allocator> {
    header: u32,
    preheader: u32,
    preheader_is_jump: bool,
    iv_inst: u32,
    iv_ty: Ty,
    start: u32,
    bound: u32,
    is_signed: bool,
    scaled_ivs: [(u32, u32); 2],
    scaled_iv_len: usize,
    pre_args: Vec<u32, A>,
    in_loop: Vec<bool, A>,
    loop_blocks: Vec<u32, A>,
}

/// Reads the argument list on a pre-tested loop's back-edge from `latch` to `header`:
/// either `Op::Jump(header, args)` or an `Op::Br` arm targeting `header` whose OTHER arm stays
/// inside `in_loop` (e.g. an inner loop header exiting directly back to an outer loop header after
/// `Range::next` jump-threading collapses empty latch blocks).
fn read_pretested_back_edge_to_header<A: Allocator + Copy>(
    view: &View<'_>,
    latch: u32,
    header: u32,
    expected_argc: usize,
    in_loop: &[bool],
    alloc: A,
) -> Option<Vec<u32, A>> {
    if latch == header {
        return None;
    }
    let term = view.terminator(Block(latch));
    if let Some((tgt, args)) = read_jump(view, term, alloc) {
        if tgt == header && args.len() == expected_argc {
            return Some(args);
        }
        return None;
    }
    if let Some(br) = read_br(view, term, alloc) {
        if br.t_blk == header
            && br.e_blk != header
            && at(in_loop, to_idx(br.e_blk))
            && br.t_args.len() == expected_argc
        {
            return Some(br.t_args);
        }
        if br.e_blk == header
            && br.t_blk != header
            && at(in_loop, to_idx(br.t_blk))
            && br.e_args.len() == expected_argc
        {
            return Some(br.e_args);
        }
    }
    None
}

/// Resolves a loop-invariant `Op::Param` on `header` (every back-edge passes the param itself) to
/// its preheader incoming value.
fn resolve_invariant_header_param<A: Allocator>(
    raw: u32,
    h_params: core::ops::Range<usize>,
    pre_args: &[u32],
    back_edges: &[Vec<u32, A>],
) -> u32 {
    let ri = to_idx(raw);
    if ri >= h_params.start && ri < h_params.end {
        let q = ri - h_params.start;
        if !back_edges.is_empty() && back_edges.iter().all(|be| at(be, q) == raw) {
            return at(pre_args, q);
        }
    }
    raw
}

/// Collects up to two secondary scaled induction variables on `header` (`start == 0`, stepping by
/// `+stride` on every back-edge with `stride` defined outside `in_loop`).
fn collect_scaled_ivs<A: Allocator + Copy>(
    view: &View<'_>,
    h_params: core::ops::Range<usize>,
    iv_pos: usize,
    iv_ty: Ty,
    pre_args: &[u32],
    back_edges: &[Vec<u32, A>],
    in_loop: &[bool],
    cfg: &CfgInfo<A>,
) -> ([(u32, u32); 2], usize) {
    let mut scaled_ivs = [(NONE, NONE); 2];
    let mut scaled_iv_len = 0usize;
    if back_edges.is_empty() {
        return (scaled_ivs, 0);
    }
    for q in 0..h_params.len() {
        if q == iv_pos {
            continue;
        }
        if scaled_iv_len >= scaled_ivs.len() {
            break;
        }
        let p_idx = h_params.start + q;
        let p_inst = to_u32(p_idx);
        if Ty(at(view.tys, p_idx)) != iv_ty {
            continue;
        }
        if const_u64(view, to_idx(at(pre_args, q))) != Some(0) {
            continue;
        }
        let mut common_stride: Option<u32> = None;
        let mut all_ok = true;
        for be in back_edges {
            let Some(st) = step_stride_of(view, at(be, q), p_inst) else {
                all_ok = false;
                break;
            };
            let st_resolved = resolve_invariant_header_param(st, h_params.start..h_params.end, pre_args, back_edges);
            if at(in_loop, to_idx(at(&cfg.inst_block, to_idx(st_resolved)))) {
                all_ok = false;
                break;
            }
            match common_stride {
                None => common_stride = Some(st_resolved),
                Some(prev) if same_val_or_const(view, prev, st_resolved) => {}
                Some(_) => {
                    all_ok = false;
                    break;
                }
            }
        }
        if all_ok && let Some(st) = common_stride {
            set_at(&mut scaled_ivs, scaled_iv_len, (p_inst, st));
            scaled_iv_len += 1;
        }
    }
    (scaled_ivs, scaled_iv_len)
}

/// Discovers all single-preheader counted natural loops (`start..bound`, step `+1`) with
/// `BlockKind::Normal` headers, plus a mask of every natural loop's own header/latch exit blocks.
fn discover_counted_loops<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &CfgInfo<A>,
    alloc: A,
) -> (Vec<CountedLoop<A>, A>, Vec<bool, A>) {
    let blocks = view.blocks();
    let mut counted: Vec<CountedLoop<A>, A> = Vec::new_in(alloc);
    let mut loop_header_or_exit_br = filled(blocks, false, &alloc);

    for h in 1..blocks {
        if at(&cfg.idom, h) == NONE {
            continue;
        }
        let header = to_u32(h);
        let Some(p_list) = cfg.preds.get(h) else {
            continue;
        };
        let mut back_latches: Vec<u32, A> = Vec::new_in(alloc);
        let mut outside_preds: Vec<u32, A> = Vec::new_in(alloc);
        for &p in p_list {
            if at(&cfg.idom, to_idx(p)) == NONE {
                continue;
            }
            if cfg.dominates(header, p) {
                back_latches.push(p);
            } else {
                outside_preds.push(p);
            }
        }
        if back_latches.is_empty() {
            continue;
        }
        // Mark back-latch `Op::Br` blocks as loop exits immediately; mark `header` too unless
        // this turns out to be a multi-block post-tested/rotated loop (where `header` is the
        // start of the body and may itself end in a hoistable bounds-check branch!).
        for &bl in &back_latches {
            if view.op(view.terminator(Block(bl))) == Op::Br {
                set_at(&mut loop_header_or_exit_br, to_idx(bl), true);
            }
        }
        let mark_header_as_exit = |mask: &mut [bool]| {
            set_at(mask, h, true);
        };

        // Only consider `BlockKind::Normal` loops (skips already-versioned `BlockKind::Cold` fallbacks).
        if view.block_kind(Block(header)) != BlockKind::Normal {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            continue;
        }
        if outside_preds.len() != 1 {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            continue;
        }
        let preheader = at(&outside_preds, 0);
        if view.block_kind(Block(preheader)) != BlockKind::Normal || !cfg.dominates(preheader, header) {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            continue;
        }

        // Collect natural loop blocks via iterative backward BFS from all `back_latches` to `header`.
        let mut in_loop = filled(blocks, false, &alloc);
        let mut loop_blocks: Vec<u32, A> = Vec::new_in(alloc);
        let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        set_at(&mut in_loop, h, true);
        loop_blocks.push(header);
        for &bl in &back_latches {
            if bl != header && !at(&in_loop, to_idx(bl)) {
                set_at(&mut in_loop, to_idx(bl), true);
                loop_blocks.push(bl);
                work.push(bl);
            }
        }
        let max_bfs = blocks.saturating_mul(4).saturating_add(8);
        let mut malformed = false;
        for _ in 0..max_bfs {
            let Some(cur) = work.pop() else {
                break;
            };
            if let Some(cp) = cfg.preds.get(to_idx(cur)) {
                for &pred in cp {
                    if at(&cfg.idom, to_idx(pred)) == NONE {
                        continue;
                    }
                    if !cfg.dominates(header, pred) {
                        malformed = true;
                        break;
                    }
                    if !at(&in_loop, to_idx(pred)) {
                        set_at(&mut in_loop, to_idx(pred), true);
                        loop_blocks.push(pred);
                        work.push(pred);
                    }
                }
            }
            if malformed {
                break;
            }
        }
        if malformed {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            continue;
        }
        loop_blocks.sort_unstable();

        // Reject loops containing landing blocks.
        if loop_blocks.iter().any(|&bb| view.block_kind(Block(bb)).is_landing()) {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            continue;
        }

        let h_params = view.params(Block(header));
        if h_params.is_empty() {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            continue;
        }

        // Read preheader edge into `header`.
        // For pre-tested `HeaderBody` loops, `preheader` may end in `Op::Jump` OR any `Op::Br`
        // (e.g. a preceding sibling loop header `z_hdr` exiting directly into `k_hdr(0)`).
        let pre_term = view.terminator(Block(preheader));
        let (pre_args, preheader_is_jump, pre_br_skip) =
            if let Some((tgt, args)) = read_jump(view, pre_term, alloc) {
                if tgt != header || args.len() != h_params.len() {
                    mark_header_as_exit(&mut loop_header_or_exit_br);
                    continue;
                }
                (args, true, None)
            } else if let Some(br) = read_br(view, pre_term, alloc) {
                if br.t_blk == header && br.e_blk != header && br.t_args.len() == h_params.len() {
                    (br.t_args, false, Some((br.cond, true, br.e_blk)))
                } else if br.e_blk == header && br.t_blk != header && br.e_args.len() == h_params.len() {
                    (br.e_args, false, Some((br.cond, false, br.t_blk)))
                } else {
                    mark_header_as_exit(&mut loop_header_or_exit_br);
                    continue;
                }
            } else {
                mark_header_as_exit(&mut loop_header_or_exit_br);
                continue;
            };

        // Check pre-tested (`HeaderBody`) loop first: every back-latch either ends in
        // `Op::Jump(header, ...)` or in an `Op::Br` whose non-header arm stays inside `in_loop`.
        let mut pretested_edges: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(back_latches.len(), alloc);
        let mut all_pretested = true;
        for &bl in &back_latches {
            if let Some(args) =
                read_pretested_back_edge_to_header(view, bl, header, h_params.len(), &in_loop, alloc)
            {
                pretested_edges.push(args);
            } else {
                all_pretested = false;
                break;
            }
        }

        if all_pretested {
            mark_header_as_exit(&mut loop_header_or_exit_br);
            let h_term = view.terminator(Block(header));
            let Some(h_br) = read_br(view, h_term, alloc) else {
                continue;
            };
            let t_in = at(&in_loop, to_idx(h_br.t_blk));
            let e_in = at(&in_loop, to_idx(h_br.e_blk));
            let (stay_is_then, header_exit) = if t_in && !e_in {
                (true, h_br.e_blk)
            } else if e_in && !t_in {
                (false, h_br.t_blk)
            } else {
                continue;
            };
            if is_panic_sink_block(view, header_exit, &in_loop) {
                continue;
            }

            let mut found = None;
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at(view.tys, p_idx));
                if p_ty != Ty::I64 && p_ty != Ty::I32 {
                    continue;
                }
                let start_v = at(&pre_args, pos);
                if !pretested_edges.iter().all(|be| is_step_one(view, at(be, pos), p_inst)) {
                    continue;
                }
                let start_is_zero = const_u64(view, to_idx(start_v)) == Some(0);
                if let Some((raw_bound, is_signed)) =
                    parse_lt_cond(view, h_br.cond, p_inst, start_is_zero, stay_is_then)
                {
                    let bound = resolve_invariant_header_param(
                        raw_bound,
                        h_params.start..h_params.end,
                        pre_args.as_slice(),
                        pretested_edges.as_slice(),
                    );
                    if !at(&in_loop, to_idx(at(&cfg.inst_block, to_idx(bound)))) {
                        found = Some((pos, p_inst, p_ty, start_v, bound, is_signed));
                        break;
                    }
                }
            }
            let Some((iv_pos, iv_inst, iv_ty, start, bound, is_signed)) = found else {
                continue;
            };
            let (scaled_ivs, scaled_iv_len) = collect_scaled_ivs(
                view,
                h_params.start..h_params.end,
                iv_pos,
                iv_ty,
                pre_args.as_slice(),
                pretested_edges.as_slice(),
                &in_loop,
                cfg,
            );
            counted.push(CountedLoop {
                header,
                preheader,
                preheader_is_jump,
                iv_inst,
                iv_ty,
                start,
                bound,
                is_signed,
                scaled_ivs,
                scaled_iv_len,
                pre_args,
                in_loop,
                loop_blocks,
            });
        } else if back_latches.len() == 1
            && let Some(l_br) = read_br(view, view.terminator(Block(at(&back_latches, 0))), alloc)
        {
            let latch = at(&back_latches, 0);
            // Post-tested / rotated loop (`SingleBlock` or multi-block with latch `Br`).
            let (stay_is_then, back_args, exit_blk) =
                if l_br.t_blk == header && !at(&in_loop, to_idx(l_br.e_blk)) && l_br.t_args.len() == h_params.len() {
                    (true, l_br.t_args, l_br.e_blk)
                } else if l_br.e_blk == header
                    && !at(&in_loop, to_idx(l_br.t_blk))
                    && l_br.e_args.len() == h_params.len()
                {
                    (false, l_br.e_args, l_br.t_blk)
                } else {
                    mark_header_as_exit(&mut loop_header_or_exit_br);
                    continue;
                };

            let single_back = [back_args];
            let mut found = None;
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at(view.tys, p_idx));
                if p_ty != Ty::I64 && p_ty != Ty::I32 {
                    continue;
                }
                let start_v = at(&pre_args, pos);
                let back_v = single_back.first().map_or(0, |be| at(be, pos));
                if !is_step_one(view, back_v, p_inst) {
                    continue;
                }
                let start_is_zero = const_u64(view, to_idx(start_v)) == Some(0);
                if let Some((raw_bound, is_signed)) =
                    parse_lt_cond(view, l_br.cond, back_v, start_is_zero, stay_is_then)
                {
                    let bound = resolve_invariant_header_param(
                        raw_bound,
                        h_params.start..h_params.end,
                        pre_args.as_slice(),
                        &single_back,
                    );
                    if !at(&in_loop, to_idx(at(&cfg.inst_block, to_idx(bound)))) {
                        found = Some((pos, p_inst, p_ty, start_v, bound, is_signed));
                        break;
                    }
                }
            }
            let Some((iv_pos, iv_inst, iv_ty, start, bound, is_signed)) = found else {
                mark_header_as_exit(&mut loop_header_or_exit_br);
                continue;
            };

            // Rotated loops must guarantee `start < bound` on entry (via preheader guard or constant).
            let entry_guarded = match pre_br_skip {
                Some((guard_cond, enter_is_then, skip_blk)) => {
                    skip_blk == exit_blk
                        && preheader_guards_entry(view, guard_cond, start, bound, is_signed, enter_is_then)
                }
                None => {
                    const_u64(view, to_idx(start)) == Some(0)
                        && const_u64(view, to_idx(bound))
                            .is_some_and(|c| c > 0 && (!is_signed || c.cast_signed() > 0))
                }
            };
            if !entry_guarded {
                mark_header_as_exit(&mut loop_header_or_exit_br);
                continue;
            }
            if latch == header {
                mark_header_as_exit(&mut loop_header_or_exit_br);
            }

            let (scaled_ivs, scaled_iv_len) = collect_scaled_ivs(
                view,
                h_params.start..h_params.end,
                iv_pos,
                iv_ty,
                pre_args.as_slice(),
                &single_back,
                &in_loop,
                cfg,
            );
            counted.push(CountedLoop {
                header,
                preheader,
                preheader_is_jump,
                iv_inst,
                iv_ty,
                start,
                bound,
                is_signed,
                scaled_ivs,
                scaled_iv_len,
                pre_args,
                in_loop,
                loop_blocks,
            });
        } else {
            mark_header_as_exit(&mut loop_header_or_exit_br);
        }
    }

    (counted, loop_header_or_exit_br)
}

// ---------------------------------------------------------------------------
// Iterative Affine Index Matcher & Overflow-Free Hoisted Guard Algebra
// ---------------------------------------------------------------------------

/// A value usable in the hoisted preheader guard (either a pure constant or an SSA value
/// strictly outside the versioned loop and dominating `outer_preheader`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TermVal {
    Const(u64),
    Inv(u32),
}

fn classify_invariant<A: Allocator + Copy>(
    view: &View<'_>,
    val: u32,
    expected_ty: Ty,
    outer_in_loop: &[bool],
    outer_preheader: u32,
    cfg: &CfgInfo<A>,
) -> Option<TermVal> {
    let vi = to_idx(val);
    if Ty(at(view.tys, vi)) != expected_ty {
        return None;
    }
    if let Some(c) = const_u64(view, vi) {
        return Some(TermVal::Const(c));
    }
    let vb = at(&cfg.inst_block, vi);
    if !at(outer_in_loop, to_idx(vb)) && cfg.dominates(vb, outer_preheader) {
        return Some(TermVal::Inv(val));
    }
    None
}

/// Active counted-loop IV descriptor available at a candidate bounds-check block.
#[derive(Clone, Copy, Debug)]
struct ActiveLoop {
    iv_inst: u32,
    iv_ty: Ty,
    start: TermVal,
    bound: TermVal,
    is_signed: bool,
    scaled_ivs: [(u32, TermVal); 2],
    scaled_iv_len: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IdxExpr {
    Const(u64),
    Inv(u32),
    Linear {
        active_idx: usize,
        offset: u64,
    },
    Scaled {
        active_idx: usize,
        stride: TermVal,
        offset: u64,
    },
    Affine2D {
        outer_active: usize,
        stride: TermVal,
        inner_active: usize,
        offset: u64,
    },
    /// Statically upper-bounded unsigned index (e.g. `(inp[i] & 15) as usize` -> `max_val = 15`).
    Bounded {
        max_val: u64,
    },
    Unknown,
}

impl IdxExpr {
    fn add(self, other: Self) -> Self {
        match (self, other) {
            (Self::Const(a), Self::Const(b)) => a.checked_add(b).map_or(Self::Unknown, Self::Const),
            (Self::Linear { active_idx, offset }, Self::Const(c))
            | (Self::Const(c), Self::Linear { active_idx, offset }) => offset
                .checked_add(c)
                .map_or(Self::Unknown, |o| Self::Linear { active_idx, offset: o }),
            (Self::Scaled { active_idx, stride, offset }, Self::Const(c))
            | (Self::Const(c), Self::Scaled { active_idx, stride, offset }) => offset
                .checked_add(c)
                .map_or(Self::Unknown, |o| Self::Scaled { active_idx, stride, offset: o }),
            (
                Self::Scaled { active_idx: outer_active, stride, offset: o1 },
                Self::Linear { active_idx: inner_active, offset: o2 },
            )
            | (
                Self::Linear { active_idx: inner_active, offset: o2 },
                Self::Scaled { active_idx: outer_active, stride, offset: o1 },
            ) if outer_active != inner_active => o1.checked_add(o2).map_or(Self::Unknown, |offset| Self::Affine2D {
                outer_active,
                stride,
                inner_active,
                offset,
            }),
            (
                Self::Affine2D { outer_active, stride, inner_active, offset },
                Self::Const(c),
            )
            | (
                Self::Const(c),
                Self::Affine2D { outer_active, stride, inner_active, offset },
            ) => offset.checked_add(c).map_or(Self::Unknown, |o| Self::Affine2D {
                outer_active,
                stride,
                inner_active,
                offset: o,
            }),
            _ => Self::Unknown,
        }
    }

    fn mul(self, other: Self) -> Self {
        match (self, other) {
            (Self::Const(a), Self::Const(b)) => a.checked_mul(b).map_or(Self::Unknown, Self::Const),
            (Self::Linear { active_idx, offset: 0 }, Self::Const(c))
            | (Self::Const(c), Self::Linear { active_idx, offset: 0 })
                if c > 0 =>
            {
                Self::Scaled { active_idx, stride: TermVal::Const(c), offset: 0 }
            }
            (Self::Linear { active_idx, offset: 0 }, Self::Inv(v))
            | (Self::Inv(v), Self::Linear { active_idx, offset: 0 }) => {
                Self::Scaled { active_idx, stride: TermVal::Inv(v), offset: 0 }
            }
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy)]
enum MatchFrame {
    Empty,
    AddWaitLhs { rhs: u32, depth: u32 },
    AddWaitRhs { lhs: IdxExpr },
    MulWaitLhs { rhs: u32, depth: u32 },
    MulWaitRhs { lhs: IdxExpr },
    ShlWait { shift_amt: u64 },
}

/// Iteratively classifies `root` as a 1D or 2D affine expression of `active_loops` (bounded depth
/// `MAX_AFFINE_DEPTH`, zero recursion).
fn eval_idx_expr<A: Allocator + Copy>(
    view: &View<'_>,
    root: u32,
    idx_ty: Ty,
    active_loops: &[ActiveLoop],
    outer_in_loop: &[bool],
    outer_preheader: u32,
    cfg: &CfgInfo<A>,
) -> IdxExpr {
    let mut frames = [MatchFrame::Empty; 16];
    let mut sp = 0usize;
    let mut cur = root;
    let mut depth = 0u32;
    let max_shift: u64 = if idx_ty == Ty::I32 { 31 } else { 63 };

    for _ in 0..64 {
        let mut res_opt: Option<IdxExpr> = None;

        // Descend until we hit a leaf or push a binary/shift frame.
        for _ in 0..16 {
            if depth > MAX_AFFINE_DEPTH {
                res_opt = Some(IdxExpr::Unknown);
                break;
            }
            let vi = to_idx(cur);
            if Ty(at(view.tys, vi)) != idx_ty {
                res_opt = Some(IdxExpr::Unknown);
                break;
            }
            if let Some(c) = const_u64(view, vi) {
                res_opt = Some(IdxExpr::Const(c));
                break;
            }
            if let Some(ai) = active_loops.iter().position(|al| al.iv_inst == cur && al.iv_ty == idx_ty) {
                res_opt = Some(IdxExpr::Linear { active_idx: ai, offset: 0 });
                break;
            }
            let mut matched_scaled = None;
            for (ai, al) in active_loops.iter().enumerate() {
                if al.iv_ty != idx_ty {
                    continue;
                }
                for &(siv, stride) in al.scaled_ivs.iter().take(al.scaled_iv_len) {
                    if siv == cur {
                        matched_scaled = Some(IdxExpr::Scaled { active_idx: ai, stride, offset: 0 });
                        break;
                    }
                }
                if matched_scaled.is_some() {
                    break;
                }
            }
            if let Some(expr) = matched_scaled {
                res_opt = Some(expr);
                break;
            }
            let vb = at(&cfg.inst_block, vi);
            if !at(outer_in_loop, to_idx(vb)) && cfg.dominates(vb, outer_preheader) {
                res_opt = Some(IdxExpr::Inv(cur));
                break;
            }
            match view.op(vi) {
                Op::Freeze => {
                    cur = at(view.data, vi);
                    depth += 1;
                }
                Op::Add => {
                    if sp >= frames.len() {
                        res_opt = Some(IdxExpr::Unknown);
                        break;
                    }
                    let pos = to_idx(at(view.data, vi));
                    let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                    set_at(&mut frames, sp, MatchFrame::AddWaitLhs { rhs: b, depth: depth + 1 });
                    sp += 1;
                    cur = a;
                    depth += 1;
                }
                Op::Mul => {
                    if sp >= frames.len() {
                        res_opt = Some(IdxExpr::Unknown);
                        break;
                    }
                    let pos = to_idx(at(view.data, vi));
                    let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                    set_at(&mut frames, sp, MatchFrame::MulWaitLhs { rhs: b, depth: depth + 1 });
                    sp += 1;
                    cur = a;
                    depth += 1;
                }
                Op::Shl => {
                    if sp >= frames.len() {
                        res_opt = Some(IdxExpr::Unknown);
                        break;
                    }
                    let pos = to_idx(at(view.data, vi));
                    let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                    let Some(sh) = const_u64(view, to_idx(b)).filter(|&k| k < max_shift) else {
                        res_opt = Some(IdxExpr::Unknown);
                        break;
                    };
                    set_at(&mut frames, sp, MatchFrame::ShlWait { shift_amt: sh });
                    sp += 1;
                    cur = a;
                    depth += 1;
                }
                _ => {
                    res_opt = Some(IdxExpr::Unknown);
                    break;
                }
            }
        }

        let mut val = res_opt.unwrap_or(IdxExpr::Unknown);

        // Unwind pending frames until we need to evaluate a RHS or finish.
        let mut descend_rhs = false;
        for _ in 0..16 {
            if sp == 0 {
                return val;
            }
            sp -= 1;
            let frame = frames.get(sp).copied().unwrap_or(MatchFrame::Empty);
            match frame {
                MatchFrame::Empty => return IdxExpr::Unknown,
                MatchFrame::ShlWait { shift_amt } => {
                    let factor = 1u64.checked_shl(trunc_u32(shift_amt)).unwrap_or(0);
                    val = if factor > 0 { val.mul(IdxExpr::Const(factor)) } else { IdxExpr::Unknown };
                }
                MatchFrame::AddWaitLhs { rhs, depth: d } => {
                    if matches!(val, IdxExpr::Unknown) {
                        continue;
                    }
                    set_at(&mut frames, sp, MatchFrame::AddWaitRhs { lhs: val });
                    sp += 1;
                    cur = rhs;
                    depth = d;
                    descend_rhs = true;
                    break;
                }
                MatchFrame::AddWaitRhs { lhs } => {
                    val = lhs.add(val);
                }
                MatchFrame::MulWaitLhs { rhs, depth: d } => {
                    if matches!(val, IdxExpr::Unknown) {
                        continue;
                    }
                    set_at(&mut frames, sp, MatchFrame::MulWaitRhs { lhs: val });
                    sp += 1;
                    cur = rhs;
                    depth = d;
                    descend_rhs = true;
                    break;
                }
                MatchFrame::MulWaitRhs { lhs } => {
                    val = lhs.mul(val);
                }
            }
        }
        if !descend_rhs {
            return val;
        }
    }
    IdxExpr::Unknown
}

/// Returns `Some(max_val)` if `root` is statically upper-bounded in `0..=max_val` (unsigned)
/// via bitmask (`And(_, mask)`), unsigned shift of a narrow integer (`Lshr(i8, k)`), or `Zext` of
/// a narrow integer (`i8` -> `255`, `i16` -> `65535`). Iterative, zero recursion.
fn proven_bounded_max(view: &View<'_>, mut cur: u32) -> Option<u64> {
    let mut best: Option<u64> = None;
    for _ in 0..4 {
        let ci = to_idx(cur);
        if ci >= view.len() {
            break;
        }
        let ty = Ty(at(view.tys, ci));
        if ty == Ty::I8 {
            best = Some(best.map_or(255u64, |b| b.min(255u64)));
        } else if ty == Ty::I16 {
            best = Some(best.map_or(65_535u64, |b| b.min(65_535u64)));
        }
        match view.op(ci) {
            Op::Zext => {
                cur = at(view.data, ci);
            }
            Op::And => {
                let pos = to_idx(at(view.data, ci));
                let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
                if let Some(m) = const_u64(view, to_idx(r)) {
                    return Some(best.map_or(m, |b| b.min(m)));
                }
                if let Some(m) = const_u64(view, to_idx(l)) {
                    return Some(best.map_or(m, |b| b.min(m)));
                }
                break;
            }
            Op::Lshr => {
                let pos = to_idx(at(view.data, ci));
                let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
                let lty = Ty(at(view.tys, to_idx(l)));
                if let Some(k) = const_u64(view, to_idx(r))
                    && lty.is_scalar()
                    && lty.is_int()
                {
                    let bits = u64::from(lty.bits(view.head.ptr_bits));
                    if bits > 0 && bits <= 64 && k < bits {
                        let type_max = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                        let shifted = type_max >> k;
                        return Some(best.map_or(shifted, |b| b.min(shifted)));
                    }
                }
                break;
            }
            _ => break,
        }
    }
    best
}

/// Returns `true` if `lhs <=u rhs` is statically guaranteed by equality, constants, `Op::Umin`,
/// or min-shaped `Op::Select` (e.g. `n = y.len().min(x.len())`). Iterative, zero recursion.
fn proves_ule(view: &View<'_>, lhs: u32, rhs: u32) -> bool {
    let mut stack = [NONE; 8];
    let mut sp = 1usize;
    set_at(&mut stack, 0, lhs);
    for _ in 0..16 {
        if sp == 0 {
            return false;
        }
        sp -= 1;
        let cur = at(&stack, sp);
        if same_val_or_const(view, cur, rhs) {
            return true;
        }
        let ci = to_idx(cur);
        if ci >= view.len() {
            continue;
        }
        if let (Some(a), Some(b)) = (const_u64(view, ci), const_u64(view, to_idx(rhs)))
            && a <= b
        {
            return true;
        }
        match view.op(ci) {
            Op::Umin => {
                let pos = to_idx(at(view.data, ci));
                let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                if sp < stack.len() {
                    set_at(&mut stack, sp, a);
                    sp += 1;
                }
                if sp < stack.len() {
                    set_at(&mut stack, sp, b);
                    sp += 1;
                }
            }
            Op::Select => {
                let pos = to_idx(at(view.data, ci));
                let (cv, tv, fv) = (
                    at(view.extra, pos),
                    at(view.extra, pos + 1),
                    at(view.extra, pos + 2),
                );
                let (inner_c, flipped) = unwrap_bool_cond(view, cv);
                let ii = to_idx(inner_c);
                if view.op(ii) == Op::Icmp {
                    let cat = to_idx(at(view.data, ii));
                    if let Some(ic) = IntCond::from_word(at(view.extra, cat)) {
                        let (x, y) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
                        let (t_eff, f_eff) = if flipped { (fv, tv) } else { (tv, fv) };
                        let is_umin = (matches!(ic, IntCond::Ult | IntCond::Ule) && t_eff == x && f_eff == y)
                            || (matches!(ic, IntCond::Ugt | IntCond::Uge) && t_eff == y && f_eff == x);
                        if is_umin {
                            if sp < stack.len() {
                                set_at(&mut stack, sp, x);
                                sp += 1;
                            }
                            if sp < stack.len() {
                                set_at(&mut stack, sp, y);
                                sp += 1;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Deduplicatable overflow-free preheader guard condition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GuardCond {
    /// 1D unit-stride (`offset == 0`) or shifted (`offset > 0`):
    /// `start <= bound` (if `start != 0`, plus `start >= 0` if signed)
    /// AND `bound <=u len` (and `(len - bound) >=u offset` when `offset > 0`).
    Bound1D {
        ty: Ty,
        start: TermVal,
        is_signed: bool,
        bound: TermVal,
        offset: u64,
        len: TermVal,
    },
    /// 2D/3D affine `iv_a * stride + iv_b + offset` (`0..bound_outer`, `0..bound_inner`):
    /// `(bound_outer empty || bound_inner empty)` OR exact zero-overflow:
    /// `max_a = bound_outer - 1`, `max_b = bound_inner - 1`,
    /// `umulhi(max_a, stride) == 0 && (max_a * stride + max_b + offset) <u len` (no wrap).
    Bound2D {
        ty: Ty,
        bound_outer: TermVal,
        outer_signed: bool,
        bound_inner: TermVal,
        inner_signed: bool,
        stride: TermVal,
        offset: u64,
        len: TermVal,
    },
    /// Statically upper-bounded index (e.g. `lut[(inp[i] & 15) as usize]`):
    /// `(bound_outer == 0) || (max_val <u len)`.
    BoundMax {
        ty: Ty,
        bound_outer: TermVal,
        max_val: u64,
        len: TermVal,
    },
}

#[inline]
fn is_const_empty_bound(tv: TermVal, ty: Ty, is_signed: bool) -> bool {
    let TermVal::Const(c) = tv else {
        return false;
    };
    if c == 0 {
        return true;
    }
    if !is_signed {
        return false;
    }
    if ty == Ty::I32 {
        trunc_u32(c).cast_signed() <= 0
    } else {
        c.cast_signed() <= 0
    }
}

fn is_guard_trivially_true(view: &View<'_>, guard: GuardCond) -> bool {
    match guard {
        GuardCond::Bound1D { start: TermVal::Const(0), offset: 0, bound, len, .. } => {
            if bound == len || bound == TermVal::Const(0) {
                return true;
            }
            match (bound, len) {
                (TermVal::Const(b), TermVal::Const(l)) => b <= l,
                (TermVal::Inv(bv), TermVal::Inv(lv)) => proves_ule(view, bv, lv),
                (TermVal::Inv(bv), TermVal::Const(l)) => {
                    const_u64(view, to_idx(bv)).is_some_and(|b| b <= l)
                }
                _ => false,
            }
        }
        GuardCond::Bound2D { ty, bound_outer, outer_signed, bound_inner, inner_signed, stride, offset, len } => {
            if is_const_empty_bound(bound_outer, ty, outer_signed)
                || is_const_empty_bound(bound_inner, ty, inner_signed)
            {
                return true;
            }
            if let (TermVal::Const(bo), TermVal::Const(bi), TermVal::Const(st), TermVal::Const(lv)) =
                (bound_outer, bound_inner, stride, len)
            {
                let max_ty: u128 = if ty == Ty::I32 {
                    u128::from(u32::MAX)
                } else {
                    u128::from(u64::MAX)
                };
                let max_a = u128::from(bo.saturating_sub(1));
                let max_b = u128::from(bi.saturating_sub(1));
                let Some(prod) = max_a.checked_mul(u128::from(st)).filter(|&p| p <= max_ty) else {
                    return false;
                };
                let Some(s1) = prod.checked_add(max_b).filter(|&s| s <= max_ty) else {
                    return false;
                };
                let Some(s2) = s1.checked_add(u128::from(offset)).filter(|&s| s <= max_ty) else {
                    return false;
                };
                return s2 < u128::from(lv);
            }
            false
        }
        GuardCond::BoundMax { bound_outer, max_val, len, .. } => {
            if bound_outer == TermVal::Const(0) {
                return true;
            }
            match len {
                TermVal::Const(l) => max_val < l,
                TermVal::Inv(lv) => const_u64(view, to_idx(lv)).is_some_and(|l| max_val < l),
            }
        }
        _ => false,
    }
}

/// One in-loop bounds-check `Op::Br` to replace with `Op::Jump(ok_blk, ok_args)` in `L_fast`.
#[derive(Clone, Copy, Debug)]
struct HoistedBranch {
    br_inst: usize,
    ok_is_then: bool,
    trap_blk: u32,
}

/// Normalizes an in-loop bounds-check `Op::Br` (`Ult`, `Ugt`, `Uge`, `Ule`, including boolean
/// negations via `Not`/`Xor(_, 1)`/`Icmp(Eq|Ne, zext(i1), 0|1)`) with one panic-sink target into
/// `(ok_is_then, trap_blk, idx, len, ty)`.
fn parse_bounds_check_br(
    view: &View<'_>,
    term: usize,
    outer_in_loop: &[bool],
) -> Option<(bool, u32, u32, u32, Ty)> {
    if view.op(term) != Op::Br {
        return None;
    }
    let pos = to_idx(at(view.data, term));
    let cond_val = at(view.extra, pos);
    let t_blk = at(view.extra, pos + 1);
    let t_argc = to_idx(at(view.extra, pos + 2));
    let e_at = pos + 3 + t_argc;
    let e_blk = at(view.extra, e_at);

    let (ok_is_then, trap_blk) = if at(outer_in_loop, to_idx(t_blk)) && is_panic_sink_block(view, e_blk, outer_in_loop) {
        (true, e_blk)
    } else if at(outer_in_loop, to_idx(e_blk)) && is_panic_sink_block(view, t_blk, outer_in_loop) {
        (false, t_blk)
    } else {
        return None;
    };

    let (inner_cond, flipped) = unwrap_bool_cond(view, cond_val);
    let ci = to_idx(inner_cond);
    if view.op(ci) != Op::Icmp || at(view.flags, ci) != 0 {
        return None;
    }
    let cat = to_idx(at(view.data, ci));
    let cond = IntCond::from_word(at(view.extra, cat))?;
    let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
    let ty = Ty(at(view.tys, to_idx(lhs)));
    if (ty != Ty::I64 && ty != Ty::I32) || Ty(at(view.tys, to_idx(rhs))) != ty {
        return None;
    }

    let check_ok_is_then = ok_is_then ^ flipped;
    let (idx, len) = if check_ok_is_then {
        match cond {
            IntCond::Ult => (lhs, rhs),
            IntCond::Ugt => (rhs, lhs),
            _ => return None,
        }
    } else {
        match cond {
            IntCond::Uge => (lhs, rhs),
            IntCond::Ule => (rhs, lhs),
            _ => return None,
        }
    };

    Some((ok_is_then, trap_blk, idx, len, ty))
}

fn build_guard_cond(expr: IdxExpr, active_loops: &[ActiveLoop], len: TermVal, ty: Ty) -> Option<GuardCond> {
    match expr {
        IdxExpr::Linear { active_idx, offset } => {
            if !fits_in_ty(offset, ty) {
                return None;
            }
            let lp = active_loops.get(active_idx).copied()?;
            Some(GuardCond::Bound1D {
                ty,
                start: lp.start,
                is_signed: lp.is_signed,
                bound: lp.bound,
                offset,
                len,
            })
        }
        IdxExpr::Scaled { active_idx, stride, offset } => {
            if !fits_in_ty(offset, ty) {
                return None;
            }
            let outer = active_loops.get(active_idx).copied()?;
            if outer.start != TermVal::Const(0) {
                return None;
            }
            Some(GuardCond::Bound2D {
                ty,
                bound_outer: outer.bound,
                outer_signed: outer.is_signed,
                bound_inner: TermVal::Const(1),
                inner_signed: false,
                stride,
                offset,
                len,
            })
        }
        IdxExpr::Affine2D { outer_active, stride, inner_active, offset } => {
            if !fits_in_ty(offset, ty) {
                return None;
            }
            let outer = active_loops.get(outer_active).copied()?;
            let inner = active_loops.get(inner_active).copied()?;
            if outer.start != TermVal::Const(0) || inner.start != TermVal::Const(0) {
                return None;
            }
            Some(GuardCond::Bound2D {
                ty,
                bound_outer: outer.bound,
                outer_signed: outer.is_signed,
                bound_inner: inner.bound,
                inner_signed: inner.is_signed,
                stride,
                offset,
                len,
            })
        }
        IdxExpr::Bounded { max_val } => {
            if !fits_in_ty(max_val, ty) {
                return None;
            }
            let outer = active_loops.first().copied()?;
            if outer.start != TermVal::Const(0) {
                return None;
            }
            Some(GuardCond::BoundMax {
                ty,
                bound_outer: outer.bound,
                max_val,
                len,
            })
        }
        IdxExpr::Const(_) | IdxExpr::Inv(_) | IdxExpr::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// Candidate Selection & Escaped-Value / Slow-Only-Sink Analysis
// ---------------------------------------------------------------------------

struct VersionPlan<A: Allocator> {
    preheader: u32,
    header: u32,
    preheader_is_jump: bool,
    pre_args: Vec<u32, A>,
    loop_blocks: Vec<u32, A>,
    in_loop: Vec<bool, A>,
    checks: Vec<HoistedBranch, A>,
    unique_guards: Vec<GuardCond, A>,
    slow_only_sinks: Vec<bool, A>,
    direct_exit: Option<u32>,
    escaped_vals: Vec<u32, A>,
}

fn find_version_plan<A: Allocator + Copy>(
    view: &View<'_>,
    opts: BceHoistOpts,
    alloc: A,
) -> Option<VersionPlan<A>> {
    let blocks = view.blocks();
    if blocks < 3 || opts.max_checks == 0 || opts.max_loop_blocks == 0 || opts.max_loop_insts == 0 {
        return None;
    }

    let cfg = CfgInfo::build(view, alloc);
    let (counted, loop_header_or_exit_br) = discover_counted_loops(view, &cfg, alloc);
    if counted.is_empty() {
        return None;
    }

    // Order candidate loops outermost-first (descending block count, deterministic tie-break on header).
    let mut order: Vec<usize, A> = Vec::with_capacity_in(counted.len(), alloc);
    for idx in 0..counted.len() {
        order.push(idx);
    }
    order.sort_unstable_by(|&a, &b| {
        let la = counted.get(a);
        let lb = counted.get(b);
        let ba = la.map_or(0, |l| l.loop_blocks.len());
        let bb = lb.map_or(0, |l| l.loop_blocks.len());
        let ha = la.map_or(0, |l| l.header);
        let hb = lb.map_or(0, |l| l.header);
        bb.cmp(&ba).then_with(|| ha.cmp(&hb))
    });

    for &cand_idx in &order {
        let Some(outer) = counted.get(cand_idx) else {
            continue;
        };

        // Enforce entry-block separation and loop-versioning budget.
        if at(&outer.in_loop, 0) || outer.loop_blocks.len() > opts.max_loop_blocks {
            continue;
        }
        let mut total_insts = 0usize;
        for &bb in &outer.loop_blocks {
            total_insts += view.block(Block(bb)).len();
        }
        if total_insts > opts.max_loop_insts {
            continue;
        }

        // Verify all `pre_args` are defined outside `outer.in_loop` and dominate `outer.preheader`.
        if outer.pre_args.iter().any(|&a| {
            let ab = at(&cfg.inst_block, to_idx(a));
            at(&outer.in_loop, to_idx(ab)) || !cfg.dominates(ab, outer.preheader)
        }) {
            continue;
        }

        // Pre-collect all counted loops nested inside `outer` whose `start` & `bound` are
        // invariant to `outer` (putting `outer` first so `active_at_blk.first()` is `outer`).
        let mut nested_loops: Vec<(usize, ActiveLoop), A> = Vec::new_in(alloc);
        let push_active = |li: usize, lp: &CountedLoop<A>, dst: &mut Vec<(usize, ActiveLoop), A>| {
            let Some(start_tv) =
                classify_invariant(view, lp.start, lp.iv_ty, &outer.in_loop, outer.preheader, &cfg)
            else {
                return;
            };
            let Some(bound_tv) =
                classify_invariant(view, lp.bound, lp.iv_ty, &outer.in_loop, outer.preheader, &cfg)
            else {
                return;
            };
            let mut scaled_ivs = [(NONE, TermVal::Const(0)); 2];
            let mut scaled_iv_len = 0usize;
            for &(siv, stride_v) in lp.scaled_ivs.iter().take(lp.scaled_iv_len) {
                if let Some(st_tv) =
                    classify_invariant(view, stride_v, lp.iv_ty, &outer.in_loop, outer.preheader, &cfg)
                {
                    set_at(&mut scaled_ivs, scaled_iv_len, (siv, st_tv));
                    scaled_iv_len += 1;
                }
            }
            dst.push((
                li,
                ActiveLoop {
                    iv_inst: lp.iv_inst,
                    iv_ty: lp.iv_ty,
                    start: start_tv,
                    bound: bound_tv,
                    is_signed: lp.is_signed,
                    scaled_ivs,
                    scaled_iv_len,
                },
            ));
        };
        push_active(cand_idx, outer, &mut nested_loops);
        for (li, lp) in counted.iter().enumerate() {
            if li == cand_idx {
                continue;
            }
            let is_nested = lp.loop_blocks.iter().all(|&bb| at(&outer.in_loop, to_idx(bb)));
            if is_nested {
                push_active(li, lp, &mut nested_loops);
            }
        }

        // Scan every block in `outer.loop_blocks` for hoistable bounds-check branches.
        let mut checks: Vec<HoistedBranch, A> = Vec::new_in(alloc);
        let mut unique_guards: Vec<GuardCond, A> = Vec::new_in(alloc);
        let mut active_at_blk: Vec<ActiveLoop, A> = Vec::with_capacity_in(nested_loops.len(), alloc);

        for &bb in &outer.loop_blocks {
            // Never mistake a natural loop's own header/latch exit branch for a bounds check!
            if at(&loop_header_or_exit_br, to_idx(bb)) {
                continue;
            }
            let term = view.terminator(Block(bb));
            let Some((ok_is_then, trap_blk, idx_val, len_val, ty)) =
                parse_bounds_check_br(view, term, &outer.in_loop)
            else {
                continue;
            };
            let Some(len_tv) = classify_invariant(view, len_val, ty, &outer.in_loop, outer.preheader, &cfg) else {
                continue;
            };

            // Active counted loops containing `bb`.
            active_at_blk.clear();
            for &(li, al) in &nested_loops {
                if counted.get(li).is_some_and(|lp| at(&lp.in_loop, to_idx(bb))) {
                    active_at_blk.push(al);
                }
            }
            if active_at_blk.is_empty() {
                continue;
            }

            let expr = match eval_idx_expr(
                view,
                idx_val,
                ty,
                active_at_blk.as_slice(),
                &outer.in_loop,
                outer.preheader,
                &cfg,
            ) {
                IdxExpr::Unknown => match proven_bounded_max(view, idx_val) {
                    Some(max_val) => IdxExpr::Bounded { max_val },
                    None => IdxExpr::Unknown,
                },
                other => other,
            };
            let Some(guard) = build_guard_cond(expr, active_at_blk.as_slice(), len_tv, ty) else {
                continue;
            };

            if checks.len() >= opts.max_checks {
                break;
            }
            checks.push(HoistedBranch { br_inst: term, ok_is_then, trap_blk });
            if !is_guard_trivially_true(view, guard) && unique_guards.scan_for(&guard).is_none() {
                unique_guards.push(guard);
            }
        }

        if checks.is_empty() {
            continue;
        }

        // Identify which outside blocks are reached ONLY via hoisted-out panic edges (`slow_only_sinks`).
        let mut fast_reaches_outside = filled(blocks, false, &alloc);
        let mut slow_reaches_outside = filled(blocks, false, &alloc);
        let mut trap_targets = filled(blocks, false, &alloc);
        for &ch in &checks {
            set_at(&mut trap_targets, to_idx(ch.trap_blk), true);
        }

        for &bb in &outer.loop_blocks {
            let term = view.terminator(Block(bb));
            let folded = checks.iter().find(|ch| ch.br_inst == term).copied();
            each(view, term, |item| {
                if let Item::Edge(edge) = item
                    && !at(&outer.in_loop, to_idx(edge.block))
                {
                    set_at(&mut slow_reaches_outside, to_idx(edge.block), true);
                    let is_folded_trap = folded.is_some_and(|ch| {
                        (ch.ok_is_then && edge.ordinal == 1) || (!ch.ok_is_then && edge.ordinal == 0)
                    });
                    if !is_folded_trap {
                        set_at(&mut fast_reaches_outside, to_idx(edge.block), true);
                    }
                }
            });
        }

        let mut slow_only_sinks = filled(blocks, false, &alloc);
        for b in 1..blocks {
            if !at(&outer.in_loop, b) && at(&trap_targets, b) && !at(&fast_reaches_outside, b) {
                let all_preds_in_loop = cfg.preds.get(b).is_some_and(|ps| {
                    let mut saw_any = false;
                    for &p in ps {
                        if at(&cfg.idom, to_idx(p)) == NONE {
                            continue;
                        }
                        saw_any = true;
                        if !at(&outer.in_loop, to_idx(p)) {
                            return false;
                        }
                    }
                    saw_any
                });
                if all_preds_in_loop {
                    set_at(&mut slow_only_sinks, b, true);
                }
            }
        }
        // Propagate `slow_only_sinks` forward along single-sink chains (bounded).
        let max_sink_rounds = blocks.min(32);
        for _ in 0..max_sink_rounds {
            let mut changed = false;
            for b in 1..blocks {
                if !at(&slow_only_sinks, b) {
                    continue;
                }
                let term = view.terminator(Block(to_u32(b)));
                each(view, term, |item| {
                    if let Item::Edge(edge) = item {
                        let nb = to_idx(edge.block);
                        if nb != 0
                            && !at(&outer.in_loop, nb)
                            && !at(&slow_only_sinks, nb)
                            && !at(&fast_reaches_outside, nb)
                        {
                            let all_preds_sink = cfg.preds.get(nb).is_some_and(|ps| {
                                let mut saw_any = false;
                                for &p in ps {
                                    if at(&cfg.idom, to_idx(p)) == NONE {
                                        continue;
                                    }
                                    saw_any = true;
                                    if !at(&slow_only_sinks, to_idx(p)) {
                                        return false;
                                    }
                                }
                                saw_any
                            });
                            if all_preds_sink {
                                set_at(&mut slow_only_sinks, nb, true);
                                changed = true;
                            }
                        }
                    }
                });
            }
            if !changed {
                break;
            }
        }

        // Collect values defined inside `outer.in_loop` directly used in non-`slow_only_sinks` outside blocks.
        let mut escaped_vals: Vec<u32, A> = Vec::new_in(alloc);
        let mut escape_use_blocks: Vec<u32, A> = Vec::new_in(alloc);
        for b in 0..blocks {
            if at(&outer.in_loop, b) || at(&slow_only_sinks, b) {
                continue;
            }
            let b32 = to_u32(b);
            for inst in view.block(Block(b32)) {
                for_each_use(view, inst, |v| {
                    if at(&outer.in_loop, to_idx(at(&cfg.inst_block, to_idx(v)))) {
                        if escaped_vals.scan_for(&v).is_none() {
                            escaped_vals.push(v);
                        }
                        if escape_use_blocks.scan_for(&b32).is_none() {
                            escape_use_blocks.push(b32);
                        }
                    }
                });
            }
        }

        let mut shared_exits: Vec<u32, A> = Vec::new_in(alloc);
        for b in 0..blocks {
            let b32 = to_u32(b);
            let is_trap_only = at(&trap_targets, b)
                && !at(&fast_reaches_outside, b)
                && escape_use_blocks.scan_for(&b32).is_none();
            if !at(&outer.in_loop, b)
                && !at(&slow_only_sinks, b)
                && !is_trap_only
                && (at(&fast_reaches_outside, b) || at(&slow_reaches_outside, b))
            {
                shared_exits.push(b32);
            }
        }

        let direct_exit = if escaped_vals.is_empty() {
            None
        } else if shared_exits.len() == 1
            && let Some(&ex) = shared_exits.first()
        {
            let ex_ok = ex != 0
                && !view.block_kind(Block(ex)).is_landing()
                && cfg
                    .preds
                    .get(to_idx(ex))
                    .is_some_and(|ps| !ps.is_empty() && ps.iter().all(|&p| at(&outer.in_loop, to_idx(p))))
                && escape_use_blocks.iter().all(|&ub| cfg.dominates(ex, ub));
            if ex_ok {
                Some(ex)
            } else {
                continue;
            }
        } else {
            continue;
        };

        return Some(VersionPlan {
            preheader: outer.preheader,
            header: outer.header,
            preheader_is_jump: outer.preheader_is_jump,
            pre_args: copy_u32_vec(&outer.pre_args, alloc),
            loop_blocks: copy_u32_vec(&outer.loop_blocks, alloc),
            in_loop: {
                let mut v = Vec::with_capacity_in(outer.in_loop.len(), alloc);
                for &b in &outer.in_loop {
                    v.push(b);
                }
                v
            },
            checks,
            unique_guards,
            slow_only_sinks,
            direct_exit,
            escaped_vals,
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Guard Emission & Loop Versioning Rewrite
// ---------------------------------------------------------------------------

#[inline]
fn emit_term_val<A: Allocator>(func: &mut Func<A>, ty: Ty, tv: TermVal, val_map_out: &[u32], loc: u32) -> u32 {
    match tv {
        TermVal::Const(c) => push_iconst(func, ty, c, loc),
        TermVal::Inv(v) => at(val_map_out, to_idx(v)),
    }
}

fn emit_guard_cond<A: Allocator>(
    func: &mut Func<A>,
    guard: GuardCond,
    val_map_out: &[u32],
    loc: u32,
) -> u32 {
    match guard {
        GuardCond::Bound1D { ty, start, is_signed, bound, offset, len } => {
            let bound_v = emit_term_val(func, ty, bound, val_map_out, loc);
            let len_v = emit_term_val(func, ty, len, val_map_out, loc);
            let mut acc = push_icmp(func, IntCond::Ule, bound_v, len_v, loc);

            if start != TermVal::Const(0) {
                let start_v = emit_term_val(func, ty, start, val_map_out, loc);
                if is_signed {
                    let zero_v = push_iconst(func, ty, 0, loc);
                    let non_neg = push_icmp(func, IntCond::Sge, start_v, zero_v, loc);
                    let ordered = push_icmp(func, IntCond::Sle, start_v, bound_v, loc);
                    let s_ok = push_bin(func, Op::And, Ty::I1, non_neg, ordered, loc);
                    acc = push_bin(func, Op::And, Ty::I1, s_ok, acc, loc);
                } else {
                    let ordered = push_icmp(func, IntCond::Ule, start_v, bound_v, loc);
                    acc = push_bin(func, Op::And, Ty::I1, ordered, acc, loc);
                }
            }

            if offset > 0 {
                let rem = push_bin(func, Op::Sub, ty, len_v, bound_v, loc);
                let off_v = push_iconst(func, ty, offset, loc);
                let off_ok = push_icmp(func, IntCond::Uge, rem, off_v, loc);
                acc = push_bin(func, Op::And, Ty::I1, acc, off_ok, loc);
            }
            acc
        }
        GuardCond::Bound2D { ty, bound_outer, outer_signed, bound_inner, inner_signed, stride, offset, len } => {
            let bo_v = emit_term_val(func, ty, bound_outer, val_map_out, loc);
            let bi_is_one = bound_inner == TermVal::Const(1);
            let bi_same_as_bo = bound_inner == bound_outer;
            let bi_v = if bi_same_as_bo {
                bo_v
            } else if bi_is_one {
                NONE
            } else {
                emit_term_val(func, ty, bound_inner, val_map_out, loc)
            };
            let st_v = if stride == bound_outer {
                bo_v
            } else if !bi_is_one && stride == bound_inner {
                bi_v
            } else {
                emit_term_val(func, ty, stride, val_map_out, loc)
            };
            let len_v = emit_term_val(func, ty, len, val_map_out, loc);

            let zero_v = push_iconst(func, ty, 0, loc);
            let bo_empty_cond = if outer_signed { IntCond::Sle } else { IntCond::Eq };
            let bo_empty = push_icmp(func, bo_empty_cond, bo_v, zero_v, loc);
            let empty = if bi_is_one || (bi_same_as_bo && inner_signed == outer_signed) {
                bo_empty
            } else {
                let bi_empty_cond = if inner_signed { IntCond::Sle } else { IntCond::Eq };
                let bi_empty = push_icmp(func, bi_empty_cond, bi_v, zero_v, loc);
                push_bin(func, Op::Or, Ty::I1, bo_empty, bi_empty, loc)
            };

            // Exact overflow-free upper-bound guard:
            //   max_a = bound_outer - 1, max_b = bound_inner - 1
            //   umulhi(max_a, stride) == 0 && (max_a * stride + max_b + offset) <u len (no wrap).
            let one_v = push_iconst(func, ty, 1, loc);
            let max_a = push_bin(func, Op::Sub, ty, bo_v, one_v, loc);
            let hi = push_bin(func, Op::Umulhi, ty, max_a, st_v, loc);
            let mul_ok = push_icmp(func, IntCond::Eq, hi, zero_v, loc);
            let prod = push_bin(func, Op::Mul, ty, max_a, st_v, loc);

            let (mut sum, mut in_bounds) = if bi_is_one {
                (prod, mul_ok)
            } else {
                let max_b = if bi_same_as_bo {
                    max_a
                } else {
                    push_bin(func, Op::Sub, ty, bi_v, one_v, loc)
                };
                let s = push_bin(func, Op::Add, ty, prod, max_b, loc);
                let add_ok = push_icmp(func, IntCond::Uge, s, prod, loc);
                (s, push_bin(func, Op::And, Ty::I1, mul_ok, add_ok, loc))
            };

            if offset > 0 {
                let off_v = push_iconst(func, ty, offset, loc);
                let s2 = push_bin(func, Op::Add, ty, sum, off_v, loc);
                let off_ok = push_icmp(func, IntCond::Uge, s2, sum, loc);
                in_bounds = push_bin(func, Op::And, Ty::I1, in_bounds, off_ok, loc);
                sum = s2;
            }

            let lt_len = push_icmp(func, IntCond::Ult, sum, len_v, loc);
            in_bounds = push_bin(func, Op::And, Ty::I1, in_bounds, lt_len, loc);
            push_bin(func, Op::Or, Ty::I1, empty, in_bounds, loc)
        }
        GuardCond::BoundMax { ty, bound_outer, max_val, len } => {
            let bo_v = emit_term_val(func, ty, bound_outer, val_map_out, loc);
            let len_v = emit_term_val(func, ty, len, val_map_out, loc);
            let zero_v = push_iconst(func, ty, 0, loc);
            let empty = push_icmp(func, IntCond::Eq, bo_v, zero_v, loc);
            let max_v = push_iconst(func, ty, max_val, loc);
            let ok = push_icmp(func, IntCond::Ult, max_v, len_v, loc);
            push_bin(func, Op::Or, Ty::I1, empty, ok, loc)
        }
    }
}

fn emit_combined_guard<A: Allocator>(
    func: &mut Func<A>,
    guards: &[GuardCond],
    val_map_out: &[u32],
    loc: u32,
) -> u32 {
    let mut combined = NONE;
    for &g in guards {
        let g_val = emit_guard_cond(func, g, val_map_out, loc);
        combined = if combined == NONE {
            g_val
        } else {
            push_bin(func, Op::And, Ty::I1, combined, g_val, loc)
        };
    }
    if combined == NONE {
        push_iconst(func, Ty::I1, 1, loc)
    } else {
        combined
    }
}

#[derive(Clone, Copy)]
enum ValFixup {
    Data { inst: usize, old_val: u32 },
    Extra { pos: usize, old_val: u32 },
}

fn resolve_val<A: Allocator>(
    val_map: &[u32],
    old_val: u32,
    extra_pos: Option<usize>,
    inst_idx: Option<usize>,
    fixups: &mut Vec<ValFixup, A>,
) -> u32 {
    let mapped = at(val_map, to_idx(old_val));
    if mapped != NONE {
        return mapped;
    }
    if let Some(pos) = extra_pos {
        fixups.push(ValFixup::Extra { pos, old_val });
    } else if let Some(inst) = inst_idx {
        fixups.push(ValFixup::Data { inst, old_val });
    }
    0
}

fn emit_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    direct_exit: Option<u32>,
    escaped_vals: &[u32],
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) -> u32 {
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let out_inst = func.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let old = at(view.data, inst);
            resolve_val(val_map, old, None, Some(out_inst), fixups)
        }
        Layout::Data(Field::Block) => at(blk_map, to_idx(at(view.data, inst))),
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        let old = at(view.extra, p);
                        let pos = func.extra.len();
                        let v = resolve_val(val_map, old, Some(pos), None, fixups);
                        func.extra.push(v);
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
                            let old = at(view.extra, p + k);
                            let pos = func.extra.len();
                            let v = resolve_val(val_map, old, Some(pos), None, fixups);
                            func.extra.push(v);
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
                            let old = at(view.extra, p + k);
                            let pos = func.extra.len();
                            let v = resolve_val(val_map, old, Some(pos), None, fixups);
                            func.extra.push(v);
                        }
                        for &ev in escaped_vals.iter().take(extra_esc) {
                            let pos = func.extra.len();
                            let v = resolve_val(val_map, ev, Some(pos), None, fixups);
                            func.extra.push(v);
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
                                let old = at(view.extra, p + k);
                                let pos = func.extra.len();
                                let v = resolve_val(val_map, old, Some(pos), None, fixups);
                                func.extra.push(v);
                            }
                            for &ev in escaped_vals.iter().take(extra_esc) {
                                let pos = func.extra.len();
                                let v = resolve_val(val_map, ev, Some(pos), None, fixups);
                                func.extra.push(v);
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

fn emit_folded_bounds_br<A: Allocator>(
    view: &View<'_>,
    br_inst: usize,
    ok_is_then: bool,
    val_map: &[u32],
    blk_map: &[u32],
    direct_exit: Option<u32>,
    escaped_vals: &[u32],
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) {
    let idx = to_idx(at(view.data, br_inst));
    let t_blk = at(view.extra, idx + 1);
    let t_argc = to_idx(at(view.extra, idx + 2));
    let e_at = idx + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));
    let (tgt_old, args_start, argc) = if ok_is_then {
        (t_blk, idx + 3, t_argc)
    } else {
        (e_blk, e_at + 2, e_argc)
    };
    let extra_esc = if direct_exit == Some(tgt_old) { escaped_vals.len() } else { 0 };
    let dst_at = to_u32(func.extra.len());
    func.extra.push(at(blk_map, to_idx(tgt_old)));
    func.extra.push(to_u32(argc + extra_esc));
    for k in 0..argc {
        let old = at(view.extra, args_start + k);
        let pos = func.extra.len();
        let v = resolve_val(val_map, old, Some(pos), None, fixups);
        func.extra.push(v);
    }
    for &ev in escaped_vals.iter().take(extra_esc) {
        let pos = func.extra.len();
        let v = resolve_val(val_map, ev, Some(pos), None, fixups);
        func.extra.push(v);
    }
    func.ops.push(op_byte(Op::Jump));
    func.flags.push(0);
    func.tys.push(Ty::NONE.0);
    func.data.push(dst_at);
    func.attrs.push(0);
    func.locs.push(at(view.locs, br_inst));
}

fn emit_redirected_br_preheader<A: Allocator>(
    view: &View<'_>,
    term: usize,
    old_header: u32,
    dispatch_blk: u32,
    val_map: &[u32],
    blk_map_outside: &[u32],
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) {
    let src = to_idx(at(view.data, term));
    let cond_old = at(view.extra, src);
    let t_blk = at(view.extra, src + 1);
    let t_argc = to_idx(at(view.extra, src + 2));
    let e_at = src + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));

    let dst = to_u32(func.extra.len());
    let cpos = func.extra.len();
    let cond_new = resolve_val(val_map, cond_old, Some(cpos), None, fixups);
    func.extra.push(cond_new);

    if t_blk == old_header {
        func.extra.push(dispatch_blk);
        func.extra.push(0);
    } else {
        func.extra.push(at(blk_map_outside, to_idx(t_blk)));
        func.extra.push(to_u32(t_argc));
        for k in 0..t_argc {
            let old = at(view.extra, src + 3 + k);
            let pos = func.extra.len();
            let v = resolve_val(val_map, old, Some(pos), None, fixups);
            func.extra.push(v);
        }
    }

    if e_blk == old_header {
        func.extra.push(dispatch_blk);
        func.extra.push(0);
    } else {
        func.extra.push(at(blk_map_outside, to_idx(e_blk)));
        func.extra.push(to_u32(e_argc));
        for k in 0..e_argc {
            let old = at(view.extra, e_at + 2 + k);
            let pos = func.extra.len();
            let v = resolve_val(val_map, old, Some(pos), None, fixups);
            func.extra.push(v);
        }
    }

    push_inst(
        func,
        Op::Br,
        Flags(at(view.flags, term)),
        Ty::NONE,
        dst,
        0,
        at(view.locs, term),
    );
}

fn emit_pre_jump<A: Allocator>(
    func: &mut Func<A>,
    target: u32,
    pre_args: &[u32],
    val_map_out: &[u32],
    loc: u32,
    fixups: &mut Vec<ValFixup, A>,
) {
    let dst = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(pre_args.len()));
    for &a in pre_args {
        let pos = func.extra.len();
        let v = resolve_val(val_map_out, a, Some(pos), None, fixups);
        func.extra.push(v);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, dst, 0, loc);
}

fn apply_fixups<A: Allocator>(func: &mut Func<A>, fixups: &[ValFixup], val_map: &[u32]) {
    for &fx in fixups {
        match fx {
            ValFixup::Data { inst, old_val } => {
                set_at(&mut func.data, inst, at(val_map, to_idx(old_val)));
            }
            ValFixup::Extra { pos, old_val } => {
                set_at(&mut func.extra, pos, at(val_map, to_idx(old_val)));
            }
        }
    }
}

/// Clones `plan.loop_blocks` into a bounds-check-free `L_fast` (`BlockKind::Normal`) and an
/// untouched `L_slow` (`BlockKind::Cold`) behind the hoisted preheader guard, then cleans up
/// with `dead` + `coalesce_cfg`.
fn version_loop_without_bounds_checks<A: Allocator + Copy>(
    v: &View<'_>,
    plan: &VersionPlan<A>,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let blocks = v.blocks();
    let len = v.len();

    // Block layout (arbitrary input block order; block 0 remains entry block 0):
    //   1. All non-loop, non-slow-only-sink blocks `0..blocks`
    //      (`preheader` jumps or branches to `dispatch_blk`; `coalesce_cfg` merges `preheader -> dispatch` when Jump)
    //   2. `dispatch_blk` (Normal): evaluates hoisted guards (every dominator of `preheader` is already mapped!)
    //   3. `fast_pre` (Normal, Op::Jump -> fast_header) + `L_fast` blocks (Normal)
    //   4. `slow_pre` (Cold, Op::Jump -> slow_header) + `L_slow` blocks (Cold)
    //   5. `slow_only_sinks` blocks (remapped with `val_map_slow` / `blk_map_slow`)
    let mut blk_map_outside = filled(blocks, NONE, &alloc);
    let mut blk_map_fast = filled(blocks, NONE, &alloc);
    let mut blk_map_slow = filled(blocks, NONE, &alloc);

    let mut next_blk = 0u32;
    for b in 0..blocks {
        if !at(&plan.in_loop, b) && !at(&plan.slow_only_sinks, b) {
            set_at(&mut blk_map_outside, b, next_blk);
            set_at(&mut blk_map_fast, b, next_blk);
            set_at(&mut blk_map_slow, b, next_blk);
            next_blk += 1;
        }
    }

    let dispatch_blk = next_blk;
    next_blk += 1;

    let fast_pre_blk = next_blk;
    next_blk += 1;
    for &lb in &plan.loop_blocks {
        set_at(&mut blk_map_fast, to_idx(lb), next_blk);
        next_blk += 1;
    }

    let slow_pre_blk = next_blk;
    next_blk += 1;
    for &lb in &plan.loop_blocks {
        set_at(&mut blk_map_slow, to_idx(lb), next_blk);
        next_blk += 1;
    }

    for b in 0..blocks {
        if at(&plan.slow_only_sinks, b) {
            set_at(&mut blk_map_outside, b, next_blk);
            set_at(&mut blk_map_fast, b, next_blk);
            set_at(&mut blk_map_slow, b, next_blk);
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
    let mut val_map_fast = filled(len, NONE, &alloc);
    let mut val_map_slow = filled(len, NONE, &alloc);

    let mut fixups_out: Vec<ValFixup, A> = Vec::new_in(alloc);
    let mut fixups_fast: Vec<ValFixup, A> = Vec::new_in(alloc);
    let mut fixups_slow: Vec<ValFixup, A> = Vec::new_in(alloc);

    let pre_term = v.terminator(Block(plan.preheader));
    let pre_loc = at(v.locs, pre_term);

    // 1. Emit all non-loop, non-slow-only-sink blocks (block 0 stays block 0).
    for b in 0..blocks {
        if at(&plan.in_loop, b) || at(&plan.slow_only_sinks, b) {
            continue;
        }
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(v.block_kinds, b));
        let range = v.block(Block(b32));
        if b32 == plan.preheader {
            let term = range.end.saturating_sub(1);
            for inst in range.start..term {
                let nv = emit_remapped(
                    v,
                    inst,
                    &val_map_out,
                    &blk_map_outside,
                    None,
                    &[],
                    &mut func,
                    &mut fixups_out,
                );
                set_at(&mut val_map_out, inst, nv);
                set_at(&mut val_map_fast, inst, nv);
                set_at(&mut val_map_slow, inst, nv);
            }
            if plan.preheader_is_jump {
                emit_pre_jump(&mut func, dispatch_blk, &[], &val_map_out, pre_loc, &mut fixups_out);
            } else {
                emit_redirected_br_preheader(
                    v,
                    term,
                    plan.header,
                    dispatch_blk,
                    &val_map_out,
                    &blk_map_outside,
                    &mut func,
                    &mut fixups_out,
                );
            }
        } else if plan.direct_exit == Some(b32) {
            let params = v.params(Block(b32));
            for inst in params.start..params.end {
                let nv = emit_remapped(
                    v,
                    inst,
                    &val_map_out,
                    &blk_map_outside,
                    None,
                    &[],
                    &mut func,
                    &mut fixups_out,
                );
                set_at(&mut val_map_out, inst, nv);
            }
            for &ev in &plan.escaped_vals {
                let ev_i = to_idx(ev);
                let new_param = push_inst(
                    &mut func,
                    Op::Param,
                    Flags::NONE,
                    Ty(at(v.tys, ev_i)),
                    0,
                    0,
                    at(v.locs, ev_i),
                );
                set_at(&mut val_map_out, ev_i, new_param);
            }
            for inst in params.end..range.end {
                let nv = emit_remapped(
                    v,
                    inst,
                    &val_map_out,
                    &blk_map_outside,
                    None,
                    &[],
                    &mut func,
                    &mut fixups_out,
                );
                set_at(&mut val_map_out, inst, nv);
            }
        } else {
            for inst in range {
                let nv = emit_remapped(
                    v,
                    inst,
                    &val_map_out,
                    &blk_map_outside,
                    None,
                    &[],
                    &mut func,
                    &mut fixups_out,
                );
                set_at(&mut val_map_out, inst, nv);
                set_at(&mut val_map_fast, inst, nv);
                set_at(&mut val_map_slow, inst, nv);
            }
        }
    }

    // 2. Emit `dispatch_blk`: all dominators of `plan.preheader` are already mapped in `val_map_out`.
    func.block_starts.push(to_u32(func.ops.len()));
    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
    let guard = emit_combined_guard(&mut func, plan.unique_guards.as_slice(), &val_map_out, pre_loc);
    push_br(&mut func, guard, fast_pre_blk, &[], slow_pre_blk, &[], pre_loc);

    // 3. Emit `fast_pre` (`BlockKind::Normal`) and `L_fast` (`BlockKind::Normal`).
    func.block_starts.push(to_u32(func.ops.len()));
    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
    emit_pre_jump(
        &mut func,
        at(&blk_map_fast, to_idx(plan.header)),
        plan.pre_args.as_slice(),
        &val_map_out,
        pre_loc,
        &mut fixups_out,
    );

    for &lb in &plan.loop_blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(v.block_kinds, to_idx(lb)));
        let range = v.block(Block(lb));
        for inst in range {
            if let Some(ch) = plan.checks.iter().find(|c| c.br_inst == inst).copied() {
                emit_folded_bounds_br(
                    v,
                    inst,
                    ch.ok_is_then,
                    &val_map_fast,
                    &blk_map_fast,
                    plan.direct_exit,
                    plan.escaped_vals.as_slice(),
                    &mut func,
                    &mut fixups_fast,
                );
            } else {
                let nv = emit_remapped(
                    v,
                    inst,
                    &val_map_fast,
                    &blk_map_fast,
                    plan.direct_exit,
                    plan.escaped_vals.as_slice(),
                    &mut func,
                    &mut fixups_fast,
                );
                set_at(&mut val_map_fast, inst, nv);
            }
        }
    }

    // 4. Emit `slow_pre` (`BlockKind::Cold`) and `L_slow` (`BlockKind::Cold`, untouched checks).
    func.block_starts.push(to_u32(func.ops.len()));
    func.block_kinds.push(block_kind_byte(BlockKind::Cold));
    emit_pre_jump(
        &mut func,
        at(&blk_map_slow, to_idx(plan.header)),
        plan.pre_args.as_slice(),
        &val_map_out,
        pre_loc,
        &mut fixups_out,
    );

    for &lb in &plan.loop_blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Cold));
        let range = v.block(Block(lb));
        for inst in range {
            let nv = emit_remapped(
                v,
                inst,
                &val_map_slow,
                &blk_map_slow,
                plan.direct_exit,
                plan.escaped_vals.as_slice(),
                &mut func,
                &mut fixups_slow,
            );
            set_at(&mut val_map_slow, inst, nv);
        }
    }

    // 5. Emit `slow_only_sinks` blocks (reached exclusively via untouched checks in `L_slow`).
    for b in 0..blocks {
        if !at(&plan.slow_only_sinks, b) {
            continue;
        }
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(v.block_kinds, b));
        let range = v.block(Block(b32));
        for inst in range {
            let nv = emit_remapped(
                v,
                inst,
                &val_map_slow,
                &blk_map_slow,
                None,
                &[],
                &mut func,
                &mut fixups_slow,
            );
            set_at(&mut val_map_slow, inst, nv);
            set_at(&mut val_map_out, inst, nv);
        }
    }

    apply_fixups(&mut func, fixups_out.as_slice(), &val_map_out);
    apply_fixups(&mut func, fixups_fast.as_slice(), &val_map_fast);
    apply_fixups(&mut func, fixups_slow.as_slice(), &val_map_slow);

    verify(func.view()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;
    coalesce_cfg(cleaned.view(), alloc)
}

// ---------------------------------------------------------------------------
// Public Entry Point
// ---------------------------------------------------------------------------

/// Hoists affine slice bounds checks (`a[i]`, `a[i + c]`, `a[i * stride + j]`) out of counted
/// 1D/2D/3D safe-Rust loops via loop versioning, collapsing the bounds-check-free fast clone
/// with `dead` + `coalesce_cfg` so downstream SIMD vectorization and Auto-SME2 GEMM passes fire.
pub fn hoist_loop_bounds_checks<A: Allocator + Copy>(
    view: View<'_>,
    opts: BceHoistOpts,
    alloc: A,
) -> Result<BceHoistOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut stats = BceHoistStats::default();

    for _ in 0..MAX_LOOPS_PER_FUNC {
        let Some(plan) = find_version_plan(&cur.view(), opts, alloc) else {
            break;
        };
        let hoisted_now = to_u32(plan.checks.len());
        cur = version_loop_without_bounds_checks(&cur.view(), &plan, alloc)?;
        stats.loops_versioned = stats.loops_versioned.saturating_add(1);
        stats.checks_hoisted = stats.checks_hoisted.saturating_add(hoisted_now);
    }

    verify(cur.view()).map_err(SimplifyError::Verify)?;
    Ok(BceHoistOutput { func: cur, stats })
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        Scalar,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Trap, Val},
        func::{Conv, IntrinsicId, SlotKind},
        simplify::{
            gemm::{GemmOpts, SME2_GEMM_F32_INTRINSIC, SME2_GEMM_I8_I32_INTRINSIC, optimize_gemm_loops},
            vectorize::auto_vectorize_loops,
        },
    };

    /// Finds an `Op::TargetIntrinsic` call matching `name` and returns its argument values.
    fn find_intrinsic_call<'a>(view: View<'a>, name: &str) -> Option<&'a [u32]> {
        for inst in 0..view.len() {
            if view.op(inst) == Op::TargetIntrinsic {
                let pos = view.data[inst] as usize;
                let id = IntrinsicId(view.extra[pos]);
                if view.intrinsic_name(id) == name.as_bytes() {
                    let n = view.extra[pos + 1] as usize;
                    return Some(&view.extra[pos + 2..pos + 2 + n]);
                }
            }
        }
        None
    }

    /// Builds a safe-Rust 1D `saxpy` (`for i in 0..n { y[i] += a * x[i]; }`) over two 32-element
    /// `f32` stack slots with explicit per-iteration slice bounds checks (`i < len_x` -> `trap 1`,
    /// `i < len_y` -> `trap 2`), returning `y[probe_idx]` for exact evaluator verification.
    fn build_safe_saxpy_f32() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64), // n
                Param::of(Ty::I64), // len_x
                Param::of(Ty::I64), // len_y
                Param::of(Ty::F32), // a
                Param::of(Ty::I64), // probe_idx
            ],
            &[Param::of(Ty::F32)],
        );
        b.set_signature(sig);

        let slot_x = b.slot(32 * 4, 4, SlotKind::Temp);
        let slot_y = b.slot(32 * 4, 4, SlotKind::Temp);
        let [entry, header, body, after_x, after_y, trap_x, trap_y, exit] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let len_x = b.param(Ty::I64);
        let len_y = b.param(Ty::I64);
        let alpha = b.param(Ty::F32);
        let probe_idx = b.param(Ty::I64);
        let px = b.slot_addr(slot_x);
        let py = b.slot_addr(slot_y);
        for k in 0..32u32 {
            let vx = b.fconst(Ty::F32, u128::from(((k as f32) * 0.5 + 1.0).to_bits()));
            let vy = b.fconst(Ty::F32, u128::from(((k as f32) * 1.25 - 3.0).to_bits()));
            b.store(Flags::NONE, vx, px, (k * 4) as i32, MemAttrs::aligned(2));
            b.store(Flags::NONE, vy, py, (k * 4) as i32, MemAttrs::aligned(2));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((header, &[zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        // Bounds check 1: `i < len_x`
        b.begin(body);
        let ok_x = b.icmp(IntCond::Ult, i, len_x);
        b.br(Flags::NONE, ok_x, (after_x, &[]), (trap_x, &[]));

        // Bounds check 2: `i < len_y`
        b.begin(after_x);
        let ok_y = b.icmp(IntCond::Ult, i, len_y);
        b.br(Flags::NONE, ok_y, (after_y, &[]), (trap_y, &[]));

        b.begin(after_y);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let p_x = b.ptr_add(Flags::NONE, px, off);
        let p_y = b.ptr_add(Flags::NONE, py, off);
        let vx = b.load(Ty::F32, Flags::NONE, p_x, 0, MemAttrs::aligned(2));
        let vy = b.load(Ty::F32, Flags::NONE, p_y, 0, MemAttrs::aligned(2));
        let prod = b.binary(Op::Fmul, Flags::NONE, alpha, vx);
        let res = b.binary(Op::Fadd, Flags::NONE, vy, prod);
        b.store(Flags::NONE, res, p_y, 0, MemAttrs::aligned(2));
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));

        b.begin(trap_x);
        b.trap(1);

        b.begin(trap_y);
        b.trap(2);

        b.begin(exit);
        let probe_off = b.binary(Op::Shl, Flags::NONE, probe_idx, two);
        let probe_ptr = b.ptr_add(Flags::NONE, py, probe_off);
        let out_v = b.load(Ty::F32, Flags::NONE, probe_ptr, 0, MemAttrs::aligned(2));
        b.ret(&[out_v]);

        b.finish_checked().unwrap()
    }

    /// Builds a safe-Rust 3D `matmul_f32` (`C[i*n + j] += A[i*n + k] * B[k*n + j]`) with all
    /// three slice bounds checks (`i*n+k < len_a`, `k*n+j < len_b`, `i*n+j < len_c`).
    fn build_safe_matmul_f32() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR), // c
                Param::of(Ty::PTR), // a
                Param::of(Ty::PTR), // b
                Param::of(Ty::I64), // n
                Param::of(Ty::I64), // len_a
                Param::of(Ty::I64), // len_b
                Param::of(Ty::I64), // len_c
            ],
            &[],
        );
        b.set_signature(sig);

        let [
            entry,
            i_hdr,
            i_body,
            k_hdr,
            k_body,
            k_after_a,
            j_hdr,
            j_body,
            j_after_b,
            j_after_c,
            k_latch,
            i_latch,
            trap_a,
            trap_b,
            trap_c,
            exit,
        ] = [(); 16].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let len_a = b.param(Ty::I64);
        let len_b = b.param(Ty::I64);
        let len_c = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

        // Bounds check 1: `i * n + k < len_a`
        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
        let ok_a = b.icmp(IntCond::Ult, a_idx, len_a);
        b.br(Flags::NONE, ok_a, (k_after_a, &[]), (trap_a, &[]));

        b.begin(k_after_a);
        let a_off = b.binary(Op::Shl, Flags::NONE, a_idx, two);
        let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
        let a_val = b.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(2));
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

        // Bounds check 2: `k * n + j < len_b`
        b.begin(j_body);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let ok_b = b.icmp(IntCond::Ult, b_idx, len_b);
        b.br(Flags::NONE, ok_b, (j_after_b, &[]), (trap_b, &[]));

        // Bounds check 3: `i * n + j < len_c` (inverted `Uge` -> `trap_c`)
        b.begin(j_after_b);
        let b_off = b.binary(Op::Shl, Flags::NONE, b_idx, two);
        let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
        let b_val = b.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(2));
        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let oob_c = b.icmp(IntCond::Uge, c_idx, len_c);
        b.br(Flags::NONE, oob_c, (trap_c, &[]), (j_after_c, &[]));

        b.begin(j_after_c);
        let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        let c_val = b.load(Ty::F32, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
        let c_new = b.ternary(Op::Fmuladd, Flags::NONE, a_val, b_val, c_val);
        b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(trap_a);
        b.trap(1);

        b.begin(trap_b);
        b.trap(2);

        b.begin(trap_c);
        b.trap(3);

        b.begin(exit);
        b.ret(&[]);

        b.finish_checked().unwrap()
    }

    #[test]
    fn hoists_1d_saxpy_bounds_checks_unlocks_neon_and_matches_eval() {
        let orig = build_safe_saxpy_f32();

        // Before hoisting, the 2 slice bounds checks prevent SIMD auto-vectorization.
        let pre_vec = auto_vectorize_loops(orig.view(), TestHeap).unwrap();
        assert_eq!(pre_vec.stats.loops_vectorized, 0);

        // Hoist both slice bounds checks out of the loop.
        let hoisted = hoist_loop_bounds_checks(orig.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
        verify(hoisted.func.view()).unwrap();
        assert_eq!(hoisted.stats.loops_versioned, 1);
        assert_eq!(hoisted.stats.checks_hoisted, 2);

        // The collapsed fast loop immediately auto-vectorizes into `<4 x f32>` NEON!
        let vec_out = auto_vectorize_loops(hoisted.func.view(), TestHeap).unwrap();
        verify(vec_out.func.view()).unwrap();
        assert_eq!(vec_out.stats.loops_vectorized, 1);
        let vv = vec_out.func.view();
        let v4f32 = Ty::vector(Scalar::F32, 4).unwrap();
        assert!(
            (0..vv.len()).any(|i| vv.op(i) == Op::Fmul && Ty(vv.tys[i]) == v4f32),
            "expected `<4 x f32>` NEON vector multiply in hoisted saxpy"
        );
        assert!(
            (0..vv.len()).any(|i| vv.op(i) == Op::Fadd && Ty(vv.tys[i]) == v4f32),
            "expected `<4 x f32>` NEON vector add in hoisted saxpy"
        );

        // Evaluator bit-exact equivalence across BOTH in-bounds AND out-of-bounds cases!
        let alpha_bits = u128::from(2.5f32.to_bits());
        for &(n, lx, ly, probe) in &[
            (0u128, 16u128, 16u128, 0u128),
            (1, 16, 16, 0),
            (7, 16, 16, 6),
            (16, 16, 16, 15),
            (20, 32, 32, 19),
            // Out-of-bounds on `x` (must trap identically with Trap::Code(1))
            (17, 16, 20, 0),
            // Out-of-bounds on `y` (must trap identically with Trap::Code(2))
            (17, 20, 12, 0),
            // Out-of-bounds on both (first check `x` hits on iteration 10 -> Trap::Code(1))
            (17, 10, 12, 0),
            // Out-of-bounds on `y` first (hits on iteration 8 -> Trap::Code(2))
            (17, 12, 8, 0),
        ] {
            let args = [
                Val::Int(n),
                Val::Int(lx),
                Val::Int(ly),
                Val::Int(alpha_bits),
                Val::Int(probe),
            ];
            let expected = eval::run_in(orig.view(), &args, 100_000, TestHeap).unwrap();
            let actual = eval::run_in(hoisted.func.view(), &args, 100_000, TestHeap).unwrap();
            assert_eq!(expected, actual, "saxpy mismatch for n={n}, lx={lx}, ly={ly}");
            if n > lx || n > ly {
                let want_code = if lx <= ly { 1 } else { 2 };
                assert_eq!(actual, Outcome::<TestHeap>::Trapped(Trap::Code(want_code)));
            }
        }
    }

    #[test]
    fn hoists_3d_safe_matmul_f32_and_unlocks_auto_sme2_gemm() {
        let orig = build_safe_matmul_f32();

        // Before hoisting, in-loop bounds checks block GEMM detection completely.
        let pre_gemm = optimize_gemm_loops(orig.view(), GemmOpts::sme2(), TestHeap).unwrap();
        assert_eq!(pre_gemm.stats.gemms_detected, 0);

        // Hoist ALL THREE 2D row-major bounds checks (`i*n+k`, `k*n+j`, `i*n+j`) in one versioning!
        let hoisted = hoist_loop_bounds_checks(orig.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
        verify(hoisted.func.view()).unwrap();
        assert_eq!(hoisted.stats.loops_versioned, 1);
        assert_eq!(hoisted.stats.checks_hoisted, 3);

        // `optimize_gemm_loops` IMMEDIATELY recognizes the collapsed fast 3D nest and lowers it
        // to Auto-SME2 `blaze.sme2.gemm_f32`!
        let sme2_out = optimize_gemm_loops(hoisted.func.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(sme2_out.func.view()).unwrap();
        assert_eq!(sme2_out.stats.gemms_detected, 1);
        assert_eq!(sme2_out.stats.sme2_lowered, 1);
        let args = find_intrinsic_call(sme2_out.func.view(), SME2_GEMM_F32_INTRINSIC)
            .expect("Auto-SME2 `blaze.sme2.gemm_f32` intrinsic emitted for safe-Rust matmul");
        assert_eq!(args.len(), 7);
    }

    #[test]
    fn hoisting_is_strictly_idempotent() {
        // Test idempotence on both 1D saxpy and 3D matmul.
        for func in [build_safe_saxpy_f32(), build_safe_matmul_f32()] {
            let first = hoist_loop_bounds_checks(func.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
            assert_eq!(first.stats.loops_versioned, 1);
            assert!(first.stats.checks_hoisted >= 2);

            let second =
                hoist_loop_bounds_checks(first.func.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
            verify(second.func.view()).unwrap();
            assert_eq!(second.stats.loops_versioned, 0, "second pass must not re-version");
            assert_eq!(second.stats.checks_hoisted, 0, "second pass must hoist 0 checks");
            assert_eq!(first.func.ops.as_slice(), second.func.ops.as_slice());
        }
    }

    #[test]
    fn deduplicates_identical_guards_and_handles_shifted_index_and_3d_eval() {
        // Shifted 1D `a[i + 2]` + duplicated `b[i]` checks (2 checks on `b`, 1 shifted check on `a`).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let slot_a = b.slot(16 * 4, 4, SlotKind::Temp);
        let slot_b = b.slot(16 * 4, 4, SlotKind::Temp);
        let [entry, header, b1, b2, b3, b4, trap1, trap2, trap3, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let len_a = b.param(Ty::I64);
        let len_b = b.param(Ty::I64);
        let pa = b.slot_addr(slot_a);
        let pb = b.slot_addr(slot_b);
        for k in 0..16u32 {
            let va = b.iconst(Ty::I32, u128::from(k * 3 + 1));
            let vb = b.iconst(Ty::I32, u128::from(k * 5 + 2));
            b.store(Flags::NONE, va, pa, (k * 4) as i32, MemAttrs::aligned(2));
            b.store(Flags::NONE, vb, pb, (k * 4) as i32, MemAttrs::aligned(2));
        }
        let zero = b.iconst(Ty::I64, 0);
        let zero32 = b.iconst(Ty::I32, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((header, &[zero, zero32]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I32);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (b1, &[]), (exit, &[acc]));

        // Check 1: `i + 2 < len_a`
        b.begin(b1);
        let idx_shift = b.binary(Op::Add, Flags::NONE, i, two);
        let ok1 = b.icmp(IntCond::Ult, idx_shift, len_a);
        b.br(Flags::NONE, ok1, (b2, &[]), (trap1, &[]));

        // Check 2: `i < len_b`
        b.begin(b2);
        let ok2 = b.icmp(IntCond::Ult, i, len_b);
        b.br(Flags::NONE, ok2, (b3, &[]), (trap2, &[]));

        // Check 3: duplicate `len_b > i` (swapped `Ugt`, same guard as Check 2!)
        b.begin(b3);
        let ok3 = b.icmp(IntCond::Ugt, len_b, i);
        b.br(Flags::NONE, ok3, (b4, &[]), (trap3, &[]));

        b.begin(b4);
        let off_a = b.binary(Op::Shl, Flags::NONE, idx_shift, two);
        let off_b = b.binary(Op::Shl, Flags::NONE, i, two);
        let ptr_a = b.ptr_add(Flags::NONE, pa, off_a);
        let ptr_b = b.ptr_add(Flags::NONE, pb, off_b);
        let va = b.load(Ty::I32, Flags::NONE, ptr_a, 0, MemAttrs::aligned(2));
        let vb = b.load(Ty::I32, Flags::NONE, ptr_b, 0, MemAttrs::aligned(2));
        let sum = b.binary(Op::Add, Flags::NONE, va, vb);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, sum);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(trap1);
        b.trap(11);
        b.begin(trap2);
        b.trap(12);
        b.begin(trap3);
        b.trap(13);

        b.begin(exit);
        let res = b.param(Ty::I32);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let hoisted = hoist_loop_bounds_checks(func.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
        verify(hoisted.func.view()).unwrap();
        assert_eq!(hoisted.stats.loops_versioned, 1);
        assert_eq!(hoisted.stats.checks_hoisted, 3);

        // Deduplication: entry block must emit ONLY 2 `Ule` comparisons (one for `len_a`, one for `len_b`)!
        let v = hoisted.func.view();
        let entry_ules = v
            .block(Block(0))
            .filter(|&k| v.op(k) == Op::Icmp && IntCond::from_word(v.extra[v.data[k] as usize]) == Some(IntCond::Ule))
            .count();
        assert_eq!(entry_ules, 2, "duplicate (bound, len_b) guard must be deduplicated");

        // Verify evaluator equivalence including boundary `n + 2 == len_a` and `n + 2 > len_a`!
        for &(n_v, la, lb) in &[
            (0u128, 2u128, 0u128),
            (1, 3, 1),
            (10, 12, 10),
            (14, 16, 16),
            // Shifted OOB: `n = 11, len_a = 12` -> `i = 10` checks `12 < 12` -> Trap::Code(11)
            (11, 12, 16),
            // `len_b` OOB: `n = 10, len_a = 16, len_b = 5` -> Trap::Code(12)
            (10, 16, 5),
        ] {
            let args = [Val::Int(n_v), Val::Int(la), Val::Int(lb)];
            let expected = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap();
            let actual = eval::run_in(hoisted.func.view(), &args, 50_000, TestHeap).unwrap();
            assert_eq!(expected, actual, "shifted mismatch for n={n_v}, la={la}, lb={lb}");
        }
    }

    #[test]
    fn hoists_panic_bounds_check_call_invoke_and_shared_sink_in_3d_matmul() {
        use crate::func::RefKind;

        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR), // c
                Param::of(Ty::PTR), // a
                Param::of(Ty::PTR), // b
                Param::of(Ty::I64), // n
                Param::of(Ty::I64), // len_a
                Param::of(Ty::I64), // len_b
                Param::of(Ty::I64), // len_c
            ],
            &[],
        );
        b.set_signature(sig);
        let panic_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[],
        );
        let panic_fn = b.reference(RefKind::Func, 0);

        let [
            entry,
            i_hdr,
            i_body,
            k_hdr,
            k_body,
            k_after_a,
            j_hdr,
            j_body,
            j_after_b,
            j_after_c,
            k_latch,
            i_latch,
            panic_call_a,
            shared_panic_invoke,
            invoke_normal_unreach,
            exit,
        ] = [(); 16].map(|_| b.create_block(BlockKind::Normal));
        let cleanup_pad = b.create_block(BlockKind::Cleanup);

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let len_a = b.param(Ty::I64);
        let len_b = b.param(Ty::I64);
        let len_c = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let four = b.iconst(Ty::I64, 4);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

        // Bounds check 1 (`Call` + `Op::Unreachable` panic sink, `Mul(idx, 4)` addressing).
        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
        let ok_a = b.icmp(IntCond::Ult, a_idx, len_a);
        b.br(Flags::NONE, ok_a, (k_after_a, &[]), (panic_call_a, &[]));

        b.begin(k_after_a);
        let a_off = b.binary(Op::Mul, Flags::NONE, a_idx, four);
        let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
        let a_val = b.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(2));
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

        // Bounds check 2 (`Uge` -> shared `Op::Invoke` panic sink!).
        b.begin(j_body);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let oob_b = b.icmp(IntCond::Uge, b_idx, len_b);
        b.br(Flags::NONE, oob_b, (shared_panic_invoke, &[b_idx, len_b]), (j_after_b, &[]));

        // Bounds check 3 (`Uge` -> same shared `Op::Invoke` panic sink!).
        b.begin(j_after_b);
        let b_off = b.binary(Op::Mul, Flags::NONE, b_idx, four);
        let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
        let b_val = b.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(2));
        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let oob_c = b.icmp(IntCond::Uge, c_idx, len_c);
        b.br(Flags::NONE, oob_c, (shared_panic_invoke, &[c_idx, len_c]), (j_after_c, &[]));

        b.begin(j_after_c);
        let c_off = b.binary(Op::Mul, Flags::NONE, c_idx, four);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        let c_val = b.load(Ty::F32, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
        let c_new = b.ternary(Op::Fmuladd, Flags::NONE, a_val, b_val, c_val);
        b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(panic_call_a);
        b.call(Flags::NONE, panic_fn, panic_sig, &[a_idx, len_a]);
        b.unreachable();

        b.begin(shared_panic_invoke);
        let p_idx = b.param(Ty::I64);
        let p_len = b.param(Ty::I64);
        b.invoke(Flags::NONE, panic_fn, panic_sig, &[p_idx, p_len], invoke_normal_unreach, cleanup_pad);

        b.begin(invoke_normal_unreach);
        b.unreachable();

        b.begin(cleanup_pad);
        let exc = b.param(Ty::PTR);
        b.resume(exc);

        b.begin(exit);
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let hoisted = hoist_loop_bounds_checks(func.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
        verify(hoisted.func.view()).unwrap();
        assert_eq!(hoisted.stats.loops_versioned, 1);
        assert_eq!(hoisted.stats.checks_hoisted, 3);

        // Collapsed fast 3D nest immediately unlocks Auto-SME2 GEMM lowering!
        let sme2_out = optimize_gemm_loops(hoisted.func.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(sme2_out.func.view()).unwrap();
        assert_eq!(sme2_out.stats.gemms_detected, 1);
        assert_eq!(sme2_out.stats.sme2_lowered, 1);
        assert!(find_intrinsic_call(sme2_out.func.view(), SME2_GEMM_F32_INTRINSIC).is_some());
    }

    #[test]
    fn eliminates_min_len_bounds_checks_without_slow_clone_and_hoists_rotated_header_check() {
        use crate::func::RefKind;

        // 1. `n = y.len().min(x.len())` via `Select(Ult(len_y, len_x), len_y, len_x)` with
        // `Call(panic_bounds_check)` + `Unreachable`: statically proven in-bounds -> zero slow clone!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR), // y
                Param::of(Ty::I64), // len_y
                Param::of(Ty::PTR), // x
                Param::of(Ty::I64), // len_x
                Param::of(Ty::F32), // a
            ],
            &[],
        );
        b.set_signature(sig);
        let panic_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[]);
        let panic_fn = b.reference(RefKind::Func, 0);

        let [entry, header, body, after_x, after_y, oob_x, oob_y, exit] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let py = b.param(Ty::PTR);
        let len_y = b.param(Ty::I64);
        let px = b.param(Ty::PTR);
        let len_x = b.param(Ty::I64);
        let alpha = b.param(Ty::F32);
        let y_lt_x = b.icmp(IntCond::Ult, len_y, len_x);
        let n = b.select(Flags::NONE, y_lt_x, len_y, len_x);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let four = b.iconst(Ty::I64, 4);
        b.jump((header, &[zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let oob1 = b.icmp(IntCond::Uge, i, len_x);
        b.br(Flags::NONE, oob1, (oob_x, &[]), (after_x, &[]));

        b.begin(after_x);
        let oob2 = b.icmp(IntCond::Uge, i, len_y);
        b.br(Flags::NONE, oob2, (oob_y, &[]), (after_y, &[]));

        b.begin(after_y);
        let off = b.binary(Op::Mul, Flags::NONE, i, four);
        let p_x = b.ptr_add(Flags::NONE, px, off);
        let p_y = b.ptr_add(Flags::NONE, py, off);
        let vx = b.load(Ty::F32, Flags::NONE, p_x, 0, MemAttrs::aligned(2));
        let vy = b.load(Ty::F32, Flags::NONE, p_y, 0, MemAttrs::aligned(2));
        let res = b.ternary(Op::Fma, Flags::REASSOC, alpha, vx, vy);
        b.store(Flags::NONE, res, p_y, 0, MemAttrs::aligned(2));
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));

        b.begin(oob_x);
        b.call(Flags::NONE, panic_fn, panic_sig, &[i, len_x]);
        b.unreachable();

        b.begin(oob_y);
        b.call(Flags::NONE, panic_fn, panic_sig, &[i, len_y]);
        b.unreachable();

        b.begin(exit);
        b.ret(&[]);
        let saxpy_min = b.finish_checked().unwrap();

        let hoisted = hoist_loop_bounds_checks(saxpy_min.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
        verify(hoisted.func.view()).unwrap();
        assert_eq!(hoisted.stats.loops_versioned, 1);
        assert_eq!(hoisted.stats.checks_hoisted, 2);
        let hv = hoisted.func.view();
        assert_eq!((0..hv.len()).filter(|&k| hv.op(k) == Op::Call).count(), 0, "slow panic calls must be dead-eliminated");
        assert_eq!((0..hv.len()).filter(|&k| hv.op(k) == Op::Unreachable).count(), 0);
        let vec_out = auto_vectorize_loops(hv, TestHeap).unwrap();
        verify(vec_out.func.view()).unwrap();
        assert_eq!(vec_out.stats.loops_vectorized, 1);

        // 2. Rotated post-tested loop whose `header` itself terminates in a bounds-check `Br`.
        let mut r = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let rsig = r.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        r.set_signature(rsig);
        let r_psig = r.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[]);
        let r_pfn = r.reference(RefKind::Func, 0);
        let [rentry, rhdr, rlatch, roob, rexit] = [(); 5].map(|_| r.create_block(BlockKind::Normal));
        r.begin(rentry);
        let rp = r.param(Ty::PTR);
        let rn = r.param(Ty::I64);
        let rlen = r.param(Ty::I64);
        let rz = r.iconst(Ty::I64, 0);
        let rone = r.iconst(Ty::I64, 1);
        let r3 = r.iconst(Ty::I64, 3);
        let has_any = r.icmp(IntCond::Ult, rz, rn);
        r.br(Flags::NONE, has_any, (rhdr, &[rz, rz]), (rexit, &[rz]));

        r.begin(rhdr);
        let ri = r.param(Ty::I64);
        let racc = r.param(Ty::I64);
        let oob = r.icmp(IntCond::Uge, ri, rlen);
        r.br(Flags::NONE, oob, (roob, &[]), (rlatch, &[]));

        r.begin(rlatch);
        let roff = r.binary(Op::Shl, Flags::NONE, ri, r3);
        let rptr = r.ptr_add(Flags::NONE, rp, roff);
        let rv = r.load(Ty::I64, Flags::NONE, rptr, 0, MemAttrs::aligned(3));
        let racc2 = r.binary(Op::Add, Flags::NONE, racc, rv);
        let ri2 = r.binary(Op::Add, Flags::NONE, ri, rone);
        let rkeep = r.icmp(IntCond::Ult, ri2, rn);
        r.br(Flags::NONE, rkeep, (rhdr, &[ri2, racc2]), (rexit, &[racc2]));

        r.begin(roob);
        r.call(Flags::NONE, r_pfn, r_psig, &[ri, rlen]);
        r.unreachable();

        r.begin(rexit);
        let rres = r.param(Ty::I64);
        r.ret(&[rres]);
        let rot = r.finish_checked().unwrap();

        let rot_hoisted = hoist_loop_bounds_checks(rot.view(), BceHoistOpts::default_sota(), TestHeap).unwrap();
        verify(rot_hoisted.func.view()).unwrap();
        assert_eq!(rot_hoisted.stats.loops_versioned, 1);
        assert_eq!(rot_hoisted.stats.checks_hoisted, 1);
    }

    /// Builds the exact `showdown_blaze_w3b` safe-Rust `matmul_f32` (`is_i8 = false`) or
    /// `matmul_i8_i32` (`is_i8 = true`) CFG:
    /// - outer `i` loop (`0..n`),
    /// - sibling row-zero-init loop `z_hdr` (`0..n`) exiting directly via `Op::Br` into `k_hdr(0)`,
    /// - middle `k` loop (`0..n`) and inner `j` loop (`0..n`),
    /// - all 4 slice bounds checks (`i*n+jz < c_len`, `i*n+k < a_len`, `k*n+j < b_len`,
    ///   `i*n+j < c_len`) branching to `DataAddr + Call(panic_bounds_check) + Unreachable` sinks.
    ///
    /// When `collapse_latches` is true, also collapses `i_latch` and `k_latch` straight into
    /// `k_hdr`'s and `j_hdr`'s `Op::Br` exit arms (matching `Range::next` + `thread_jumps`).
    fn build_showdown_matmul<A: Allocator + Copy>(
        is_i8: bool,
        collapse_latches: bool,
        alloc: A,
    ) -> Func<A> {
        use crate::func::RefKind;

        let mut b = Builder::<A, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR), // c
                Param::of(Ty::PTR), // a
                Param::of(Ty::PTR), // b
                Param::of(Ty::I64), // n
                Param::of(Ty::I64), // a_len
                Param::of(Ty::I64), // b_len
                Param::of(Ty::I64), // c_len
            ],
            &[],
        );
        b.set_signature(sig);
        let panic_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::PTR)],
            &[],
        );
        let panic_fn = b.reference(RefKind::Func, 0);
        let d_loc0 = b.data(b"matmul_loc_0", 0, &[]);
        let d_loc1 = b.data(b"matmul_loc_1", 0, &[]);
        let d_loc2 = b.data(b"matmul_loc_2", 0, &[]);
        let d_loc3 = b.data(b"matmul_loc_3", 0, &[]);

        let [
            entry,
            i_hdr,
            i_body,
            z_hdr,
            z_body,
            z_ok,
            k_hdr,
            k_body,
            k_ok,
            j_hdr,
            j_body,
            j_ok1,
            j_ok2,
            panic0,
            panic1,
            panic2,
            panic3,
            exit,
        ] = [(); 18].map(|_| b.create_block(BlockKind::Normal));
        let k_latch = if collapse_latches { None } else { Some(b.create_block(BlockKind::Normal)) };
        let i_latch = if collapse_latches { None } else { Some(b.create_block(BlockKind::Normal)) };

        let acc_ty = if is_i8 { Ty::I32 } else { Ty::F32 };

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let a_len = b.param(Ty::I64);
        let b_len = b.param(Ty::I64);
        let c_len = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        let zero_elem = if is_i8 { b.iconst(Ty::I32, 0) } else { b.fconst(Ty::F32, 0) };
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_next_early = if collapse_latches { Some(b.binary(Op::Add, Flags::NONE, i, one)) } else { None };
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((z_hdr, &[zero]));

        // Sibling zero-init loop exiting DIRECTLY via `Op::Br` into `k_hdr(0)`!
        b.begin(z_hdr);
        let jz = b.param(Ty::I64);
        let keep_jz = b.icmp(IntCond::Ult, jz, n);
        b.br(Flags::NONE, keep_jz, (z_body, &[]), (k_hdr, &[zero]));

        // Check 1: `i*n + jz < c_len`
        b.begin(z_body);
        let idx0 = b.binary(Op::Add, Flags::NONE, i_n, jz);
        let oob0 = b.icmp(IntCond::Uge, idx0, c_len);
        b.br(Flags::NONE, oob0, (panic0, &[]), (z_ok, &[]));

        b.begin(z_ok);
        let z_off = b.binary(Op::Shl, Flags::NONE, idx0, two);
        let z_ptr = b.ptr_add(Flags::NONE, pc, z_off);
        b.store(Flags::NONE, zero_elem, z_ptr, 0, MemAttrs::aligned(2));
        let jz2 = b.binary(Op::Add, Flags::NONE, jz, one);
        b.jump((z_hdr, &[jz2]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        if let Some(i_next) = i_next_early {
            b.br(Flags::NONE, keep_k, (k_body, &[]), (i_hdr, &[i_next]));
        } else {
            b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch.unwrap(), &[]));
        }

        // Check 2: `i*n + k < a_len`
        b.begin(k_body);
        let k_next_early = if collapse_latches { Some(b.binary(Op::Add, Flags::NONE, k, one)) } else { None };
        let idx1 = b.binary(Op::Add, Flags::NONE, i_n, k);
        let oob1 = b.icmp(IntCond::Uge, idx1, a_len);
        b.br(Flags::NONE, oob1, (panic1, &[]), (k_ok, &[]));

        b.begin(k_ok);
        let aik = if is_i8 {
            let a_ptr = b.ptr_add(Flags::NONE, pa, idx1);
            let a_raw = b.load(Ty::I8, Flags::NONE, a_ptr, 0, MemAttrs::aligned(0));
            b.convert(Op::Sext, Flags::NONE, Ty::I32, a_raw)
        } else {
            let a_off = b.binary(Op::Shl, Flags::NONE, idx1, two);
            let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
            b.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(2))
        };
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        if let Some(k_next) = k_next_early {
            b.br(Flags::NONE, keep_j, (j_body, &[]), (k_hdr, &[k_next]));
        } else {
            b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch.unwrap(), &[]));
        }

        // Check 3: `k*n + j < b_len`
        b.begin(j_body);
        let idx2 = b.binary(Op::Add, Flags::NONE, k_n, j);
        let oob2 = b.icmp(IntCond::Uge, idx2, b_len);
        b.br(Flags::NONE, oob2, (panic2, &[]), (j_ok1, &[]));

        // Check 4: `i*n + j < c_len` (identical guard to Check 1 -> deduplicated!)
        b.begin(j_ok1);
        let bkj = if is_i8 {
            let b_ptr = b.ptr_add(Flags::NONE, pb, idx2);
            let b_raw = b.load(Ty::I8, Flags::NONE, b_ptr, 0, MemAttrs::aligned(0));
            b.convert(Op::Sext, Flags::NONE, Ty::I32, b_raw)
        } else {
            let b_off = b.binary(Op::Shl, Flags::NONE, idx2, two);
            let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
            b.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(2))
        };
        let idx3 = b.binary(Op::Add, Flags::NONE, i_n, j);
        let oob3 = b.icmp(IntCond::Uge, idx3, c_len);
        b.br(Flags::NONE, oob3, (panic3, &[]), (j_ok2, &[]));

        b.begin(j_ok2);
        let c_off = b.binary(Op::Shl, Flags::NONE, idx3, two);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        let c_val = b.load(acc_ty, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
        let c_new = if is_i8 {
            let prod = b.binary(Op::Mul, Flags::NONE, aik, bkj);
            b.binary(Op::Add, Flags::NONE, c_val, prod)
        } else {
            b.ternary(Op::Fma, Flags::NONE, aik, bkj, c_val)
        };
        b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        if let Some(kl) = k_latch {
            b.begin(kl);
            let k2 = b.binary(Op::Add, Flags::NONE, k, one);
            b.jump((k_hdr, &[k2]));
        }
        if let Some(il) = i_latch {
            b.begin(il);
            let i2 = b.binary(Op::Add, Flags::NONE, i, one);
            b.jump((i_hdr, &[i2]));
        }

        b.begin(panic0);
        let loc0 = b.data_addr(d_loc0);
        b.call(Flags::NONE, panic_fn, panic_sig, &[idx0, c_len, loc0]);
        b.unreachable();

        b.begin(panic1);
        let loc1 = b.data_addr(d_loc1);
        b.call(Flags::NONE, panic_fn, panic_sig, &[idx1, a_len, loc1]);
        b.unreachable();

        b.begin(panic2);
        let loc2 = b.data_addr(d_loc2);
        b.call(Flags::NONE, panic_fn, panic_sig, &[idx2, b_len, loc2]);
        b.unreachable();

        b.begin(panic3);
        let loc3 = b.data_addr(d_loc3);
        b.call(Flags::NONE, panic_fn, panic_sig, &[idx3, c_len, loc3]);
        b.unreachable();

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn hoists_showdown_matmul_f32_and_i8_sibling_zero_init_br_preheader_to_sme2() {
        let alloc = blaze_own::testing::heap().alloc();

        // 1. Exact `showdown_blaze_w3b` CFG for both `matmul_f32` and `matmul_i8_i32`.
        for (is_i8, want_intrinsic) in [
            (false, SME2_GEMM_F32_INTRINSIC),
            (true, SME2_GEMM_I8_I32_INTRINSIC),
        ] {
            let orig = build_showdown_matmul(is_i8, false, alloc);
            let hoisted = hoist_loop_bounds_checks(orig.view(), BceHoistOpts::default_sota(), alloc).unwrap();
            verify(hoisted.func.view()).unwrap();
            assert_eq!(hoisted.stats.loops_versioned, 1, "outer i_hdr must be versioned once (is_i8={is_i8})");
            assert_eq!(hoisted.stats.checks_hoisted, 4, "all 4 bounds checks must hoist (is_i8={is_i8})");

            let post_dead = dead(hoisted.func.view(), alloc).unwrap();
            let post_coal = coalesce_cfg(post_dead.view(), alloc).unwrap();
            verify(post_coal.view()).unwrap();

            // Check 1 (`z_body`) and Check 4 (`j_ok1`) both guard `c_len` and deduplicate to 3 guards.
            let entry_umulhi = post_coal
                .view()
                .block(Block(0))
                .filter(|&inst| post_coal.view().op(inst) == Op::Umulhi)
                .count();
            assert_eq!(entry_umulhi, 3, "expected 3 deduplicated 2D Umulhi guards in entry (is_i8={is_i8})");

            let gemm = optimize_gemm_loops(post_coal.view(), GemmOpts::sme2(), alloc).unwrap();
            verify(gemm.func.view()).unwrap();
            assert_eq!(gemm.stats.gemms_detected, 1, "GEMM must be detected (is_i8={is_i8})");
            assert_eq!(gemm.stats.sme2_lowered, 1, "SME2 GEMM must lower (is_i8={is_i8})");
            let args = find_intrinsic_call(gemm.func.view(), want_intrinsic)
                .expect("expected Auto-SME2 intrinsic call");
            assert_eq!(args.len(), 7);
            assert_eq!(
                const_u64(&gemm.func.view(), args[6] as usize),
                Some(0),
                "sibling zero-init matmul must lower with flags = 0 (overwrite mode, is_i8={is_i8})"
            );
        }

        // 2. Collapsed-latch `Range::next` + `thread_jumps` shape (where `k_hdr` branches directly
        //    back to `i_hdr(i + 1)` and `j_hdr` branches directly back to `k_hdr(k + 1)`).
        for is_i8 in [false, true] {
            let collapsed = build_showdown_matmul(is_i8, true, alloc);
            let hoisted = hoist_loop_bounds_checks(collapsed.view(), BceHoistOpts::default_sota(), alloc).unwrap();
            verify(hoisted.func.view()).unwrap();
            assert_eq!(hoisted.stats.loops_versioned, 1);
            assert_eq!(hoisted.stats.checks_hoisted, 4);
        }

        // 3. Evaluator bit-exact equivalence for exact `Bound2D` guard `(bound_a-1)*stride + (bound_b-1) < len`,
        //    including `bound_inner > stride` (overlapping window) and exact OOB trap preservation.
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let slot = b.slot(32 * 4, 4, SlotKind::Temp);
        let [entry, o_hdr, o_body, i_hdr, i_body, i_ok, o_latch, trap_blk, exit] =
            [(); 9].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let m = b.param(Ty::I64);
        let k = b.param(Ty::I64);
        let stride = b.param(Ty::I64);
        let len = b.param(Ty::I64);
        let base = b.slot_addr(slot);
        for idx in 0..32u32 {
            let v = b.iconst(Ty::I32, u128::from(idx * 7 + 3));
            b.store(Flags::NONE, v, base, (idx * 4) as i32, MemAttrs::aligned(2));
        }
        let zero = b.iconst(Ty::I64, 0);
        let zero32 = b.iconst(Ty::I32, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((o_hdr, &[zero, zero32]));

        b.begin(o_hdr);
        let i = b.param(Ty::I64);
        let acc_o = b.param(Ty::I32);
        let keep_o = b.icmp(IntCond::Ult, i, m);
        b.br(Flags::NONE, keep_o, (o_body, &[]), (exit, &[acc_o]));

        b.begin(o_body);
        let row = b.binary(Op::Mul, Flags::NONE, i, stride);
        b.jump((i_hdr, &[zero, acc_o]));

        b.begin(i_hdr);
        let j = b.param(Ty::I64);
        let acc_i = b.param(Ty::I32);
        let keep_i = b.icmp(IntCond::Ult, j, k);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (o_latch, &[acc_i]));

        b.begin(i_body);
        let idx = b.binary(Op::Add, Flags::NONE, row, j);
        let oob = b.icmp(IntCond::Uge, idx, len);
        b.br(Flags::NONE, oob, (trap_blk, &[]), (i_ok, &[]));

        b.begin(i_ok);
        let off = b.binary(Op::Shl, Flags::NONE, idx, two);
        let ptr = b.ptr_add(Flags::NONE, base, off);
        let val = b.load(Ty::I32, Flags::NONE, ptr, 0, MemAttrs::aligned(2));
        let acc_next = b.binary(Op::Add, Flags::NONE, acc_i, val);
        let j_next = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((i_hdr, &[j_next, acc_next]));

        b.begin(o_latch);
        let acc_done = b.param(Ty::I32);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((o_hdr, &[i_next, acc_done]));

        b.begin(trap_blk);
        b.trap(42);

        b.begin(exit);
        let out = b.param(Ty::I32);
        b.ret(&[out]);
        let win_fn = b.finish_checked().unwrap();

        let win_hoisted = hoist_loop_bounds_checks(win_fn.view(), BceHoistOpts::default_sota(), alloc).unwrap();
        verify(win_hoisted.func.view()).unwrap();
        assert_eq!(win_hoisted.stats.loops_versioned, 1);
        assert_eq!(win_hoisted.stats.checks_hoisted, 1);

        for &(mv, kv, sv, lv) in &[
            (0u128, 5u128, 2u128, 0u128),  // empty outer
            (4, 0, 2, 0),                  // empty inner
            (4, 5, 2, 11),                 // exact boundary: (4-1)*2 + (5-1) = 10 < 11 (`k > stride`!)
            (4, 5, 2, 10),                 // OOB on last iteration: 10 >= 10 -> Trap::Code(42)
            (3, 4, 4, 12),                 // standard tight row-major: (3-1)*4 + (4-1) = 11 < 12
            (3, 4, 4, 11),                 // OOB: 11 >= 11 -> Trap::Code(42)
        ] {
            let args = [Val::Int(mv), Val::Int(kv), Val::Int(sv), Val::Int(lv)];
            let expected = eval::run_in(win_fn.view(), &args, 50_000, alloc).unwrap();
            let actual = eval::run_in(win_hoisted.func.view(), &args, 50_000, alloc).unwrap();
            assert_eq!(expected, actual, "2D guard mismatch for m={mv}, k={kv}, s={sv}, len={lv}");
        }
    }
}

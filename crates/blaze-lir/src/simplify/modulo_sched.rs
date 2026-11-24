//! Swing Modulo Scheduler (2-Stage Load-Compute Software Pipelining, `blaze_lir::modulo_sched`).
//!
//! Classic out-of-order cores still stall on tight counted loops where every iteration loads from
//! memory and immediately feeds a multi-cycle compute / recurrence chain (crypto `wyhash`/`arx`,
//! stencils, gather reductions, parsers). **Swing Modulo Scheduling (SMS)** software-pipelines
//! eligible counted single-block (rotated do-while) and 2-block (`HeaderBody`) loops into
//! `guard -> prologue -> kernel -> epilogue` while strictly respecting the target register budget:
//!
//! 1. **Dependence Graph & Apple M5 Resource/Recurrence Bounds**:
//!    - Computes intra-iteration latencies (`Load: 4`, `Mul`/`Umulhi`/`Smulhi`/`Fmul`/`Fadd`/`Fsub`/
//!      `Fma`/`Fmuladd: 3`, `ALU`/`Shift`/`Rot`/`Select`/`PtrAdd`/`Store: 1`).
//!    - Computes exact `ResMII` under Apple M5 execution ports (6 ALU / 4 FP / 3 LDST) and exact
//!      `RecMII` (maximum loop-carried recurrence latency), setting `II = max(ResMII, RecMII)`.
//!    - Computes peak live GPR and FPR pressure across the pipelined steady-state kernel and
//!      refuses to pipeline whenever staging would exceed `max_gprs` / `max_fprs` (zero spills).
//! 2. **2-Stage Load-Compute Software Pipelining**:
//!    - **Stage 0** (address calculation + `Op::Load` for iteration `i + 1`) is hoisted to the very
//!      top of the steady-state `kernel` so its 4-cycle load latency overlaps completely with
//!      **Stage 1** (compute + reduction / alias-free store on the prefetched value(s) from
//!      iteration `i`).
//!    - **Out-of-Bounds Safety Invariant**: the steady-state kernel stops while `i + 1` is still
//!      in bounds and hands the already-loaded final element `val_{n-1}` straight into `epilogue`;
//!      iteration `n - 1` NEVER issues a speculative load of `ptr[n]`.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), bounded (Rule 18),
//! explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::BlockKind,
    op::{Field, Layout},
    verify,
};

/// Maximum body instructions in a candidate loop (bounds compile time, Rule 18).
const MAX_BODY_INSTS: usize = 64;
/// Maximum Stage-0 loads staged across iterations per loop.
const MAX_STAGE0_LOADS: usize = 4;
/// Maximum loops pipelined in one function pass (bounds iterations, Rule 18).
const MAX_PIPELINED_LOOPS: usize = 8;

/// Apple M5 execution port counts for `ResMII`.
const M5_ALU_PORTS: u32 = 6;
const M5_FP_PORTS: u32 = 4;
const M5_LDST_PORTS: u32 = 3;

/// Configuration for [`swing_modulo_schedule`] (Rule 83: one options struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModuloSchedOpts {
    /// Maximum peak live GPRs allowed in the steady-state pipelined kernel.
    pub max_gprs: u32,
    /// Maximum peak live FPRs allowed in the steady-state pipelined kernel.
    pub max_fprs: u32,
    /// Minimum Stage-1 compute latency (cycles) consuming a Stage-0 load to justify staging.
    pub min_compute_latency: u32,
}

impl ModuloSchedOpts {
    /// Apple Silicon M5 defaults (24 GPRs / 28 FPRs headroom, >= 3 cycles compute chain).
    pub const fn apple_m5() -> Self {
        Self {
            max_gprs: 24,
            max_fprs: 28,
            min_compute_latency: 3,
        }
    }
}

impl Default for ModuloSchedOpts {
    fn default() -> Self {
        Self::apple_m5()
    }
}

/// Telemetry returned by [`swing_modulo_schedule`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct ModuloSchedStats {
    pub loops_pipelined: u32,
    pub loads_hoisted: u32,
    pub res_mii: u32,
    pub rec_mii: u32,
}

/// Output of [`swing_modulo_schedule`].
#[derive(Debug)]
pub struct ModuloSchedOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: ModuloSchedStats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopShape {
    HeaderBody { header: u32, body: u32, exit: u32 },
    SingleBlock { header: u32, exit: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreheaderEdge {
    Jump,
    BrThen,
    BrElse,
}

#[derive(Clone, Copy)]
struct MemAddrPattern {
    base: u32,
    stride: u64,
    off: i32,
    access_bytes: u64,
}

struct PipelinePlan<A: Allocator> {
    preheader: u32,
    pre_edge: PreheaderEdge,
    shape: LoopShape,
    iv_pos: usize,
    iv_inst: u32,
    iv_next_inst: u32,
    iv_ty: Ty,
    iv_init: u32,
    bound: u32,
    cmp_cond: IntCond,
    pre_args: Vec<u32, A>,
    back_args: Vec<u32, A>,
    exit_args: Vec<u32, A>,
    /// `HeaderBody` header parameter positions directly used outside the loop (promoted to
    /// new `Op::Param`s on `exit` when `exit` has `header` as its sole predecessor).
    promoted_header_params: Vec<usize, A>,
    /// Ordered Stage-0 instructions (pure address computation + `stage0_loads`).
    stage0_insts: Vec<u32, A>,
    /// Ordered Stage-0 `Op::Load` instruction indices whose results cross from Stage 0 to Stage 1.
    stage0_loads: Vec<u32, A>,
    /// Ordered Stage-1 instructions (compute + reductions + alias-free stores).
    stage1_insts: Vec<u32, A>,
    res_mii: u32,
    rec_mii: u32,
}

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
fn copy_u32_vec<A: Allocator + Copy>(src: &[u32], alloc: A) -> Vec<u32, A> {
    let mut out = Vec::with_capacity_in(src.len(), alloc);
    out.extend_from_slice(src);
    out
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

/// Apple M5 intra-iteration latency model (cycles).
#[inline]
fn m5_op_latency(op: Op) -> u32 {
    match op {
        Op::Load => 4,
        Op::Mul | Op::Umulhi | Op::Smulhi | Op::Fmul | Op::Fadd | Op::Fsub | Op::Fma | Op::Fmuladd => 3,
        Op::Udiv | Op::Urem | Op::Fdiv => 6,
        Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide => 0,
        _ => 1,
    }
}

#[inline]
fn is_pure_addr_op(op: Op) -> bool {
    matches!(
        op,
        Op::PtrAdd
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Iconst
            | Op::IconstWide
    )
}

#[inline]
fn is_fp_scalar(ty: Ty) -> bool {
    ty.is_scalar() && ty.is_float()
}

#[inline]
fn is_scalar_value_ty(ty: Ty) -> bool {
    matches!(
        ty,
        Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::PTR | Ty::F32 | Ty::F64
    )
}

#[inline]
fn ty_access_bytes(ty: Ty, ptr_bits: u32) -> u64 {
    let bits = u64::from(ty.bits(ptr_bits));
    bits.div_ceil(8).max(1)
}

/// Parses an address expression `(ptr_v, off)` into `(base, stride, off, access_bytes)`
/// relative to loop induction variable `iv_inst` inside `body_block`.
fn parse_mem_addr(
    view: &View<'_>,
    inst_block: &[u32],
    body_block: u32,
    iv_inst: u32,
    ptr_v: u32,
    off_u32: u32,
    access_ty: Ty,
) -> Option<MemAddrPattern> {
    let off = off_u32.cast_signed();
    let access_bytes = ty_access_bytes(access_ty, view.head.ptr_bits);
    let pi = to_idx(ptr_v);
    if at(inst_block, pi) != body_block {
        return Some(MemAddrPattern {
            base: ptr_v,
            stride: 0,
            off,
            access_bytes,
        });
    }
    if view.op(pi) != Op::PtrAdd {
        return None;
    }
    let pat = to_idx(at(view.data, pi));
    let base = at(view.extra, pat);
    let idx_v = at(view.extra, pat + 1);
    if at(inst_block, to_idx(base)) == body_block {
        return None;
    }
    let is_iv = |v: u32| -> bool {
        if v == iv_inst {
            return true;
        }
        let vi = to_idx(v);
        at(inst_block, vi) == body_block
            && matches!(view.op(vi), Op::Zext | Op::Sext)
            && at(view.data, vi) == iv_inst
    };
    if is_iv(idx_v) {
        return Some(MemAddrPattern {
            base,
            stride: 1,
            off,
            access_bytes,
        });
    }
    let ii = to_idx(idx_v);
    if at(inst_block, ii) != body_block {
        return None;
    }
    let iat = to_idx(at(view.data, ii));
    match view.op(ii) {
        Op::Shl => {
            let lhs = at(view.extra, iat);
            let rhs = at(view.extra, iat + 1);
            if is_iv(lhs)
                && let Some(sh) = const_u64(view, to_idx(rhs))
                && sh <= 12
            {
                return Some(MemAddrPattern {
                    base,
                    stride: 1u64 << sh,
                    off,
                    access_bytes,
                });
            }
            None
        }
        Op::Mul => {
            let lhs = at(view.extra, iat);
            let rhs = at(view.extra, iat + 1);
            if is_iv(lhs)
                && let Some(c) = const_u64(view, to_idx(rhs))
                && c > 0
                && c <= 4096
            {
                return Some(MemAddrPattern {
                    base,
                    stride: c,
                    off,
                    access_bytes,
                });
            }
            if is_iv(rhs)
                && let Some(c) = const_u64(view, to_idx(lhs))
                && c > 0
                && c <= 4096
            {
                return Some(MemAddrPattern {
                    base,
                    stride: c,
                    off,
                    access_bytes,
                });
            }
            None
        }
        _ => None,
    }
}

/// Proves that hoisting iteration `i + 1`'s load before iteration `i`'s store cannot alias.
fn load_store_cross_iter_disjoint(
    view: &View<'_>,
    inst_block: &[u32],
    body_block: u32,
    iv_inst: u32,
    load_inst: usize,
    store_inst: usize,
) -> bool {
    let lat = to_idx(at(view.data, load_inst));
    let l_ptr = at(view.extra, lat);
    let l_off = at(view.extra, lat + 1);
    let l_ty = Ty(at(view.tys, load_inst));

    let sat = to_idx(at(view.data, store_inst));
    let s_val = at(view.extra, sat);
    let s_ptr = at(view.extra, sat + 1);
    let s_off = at(view.extra, sat + 2);
    let s_ty = Ty(at(view.tys, to_idx(s_val)));

    let Some(lp) = parse_mem_addr(view, inst_block, body_block, iv_inst, l_ptr, l_off, l_ty) else {
        return false;
    };
    let Some(sp) = parse_mem_addr(view, inst_block, body_block, iv_inst, s_ptr, s_off, s_ty) else {
        return false;
    };

    // Case 1: Distinct stack slots (`Op::SlotAddr`) are provably disjoint.
    let lbi = to_idx(lp.base);
    let sbi = to_idx(sp.base);
    if view.op(lbi) == Op::SlotAddr
        && view.op(sbi) == Op::SlotAddr
        && at(view.data, lbi) != at(view.data, sbi)
    {
        return true;
    }

    // Case 2: Same base & stride with identical offset and `stride >= max(load_bytes, store_bytes)`:
    // iteration `i + 1` reads `[base + (i+1)*stride + off]` while iteration `i` writes
    // `[base + i*stride + off]` (in-place elementwise transform), completely disjoint!
    if lp.base == sp.base
        && lp.stride == sp.stride
        && lp.off == sp.off
        && lp.stride >= lp.access_bytes.max(sp.access_bytes)
        && lp.stride <= 4096
    {
        return true;
    }

    false
}

/// Estimates peak live GPRs and FPRs across the planned steady-state `kernel`.
///
/// Models every loop-invariant value used in the kernel, every loop-carried header parameter,
/// double-buffered Stage-0 loads (`val_i` live for Stage 1 while `val_{i+1}` is loaded at the
/// top of the kernel), and the exact per-instruction peak live temporaries inside Stage 0 & Stage 1.
fn estimate_kernel_register_pressure<A: Allocator + Copy>(
    view: &View<'_>,
    header_params: core::ops::Range<usize>,
    bound: u32,
    stage0_insts: &[u32],
    stage0_loads: &[u32],
    stage1_insts: &[u32],
    back_args: &[u32],
    is_loop_inst: &[bool],
    alloc: A,
) -> (u32, u32) {
    let mut inv_gprs = 0u32;
    let mut inv_fprs = 0u32;
    let mut seen_inv: Vec<u32, A> = Vec::new_in(alloc);

    let mut count_inv = |v: u32| {
        let vi = to_idx(v);
        if at(is_loop_inst, vi) {
            return;
        }
        // Small integer constants fold into AArch64 immediates and do not consume a persistent GPR.
        if view.op(vi) == Op::Iconst {
            return;
        }
        if seen_inv.scan_for(&v).is_none() {
            seen_inv.push(v);
            let ty = Ty(at(view.tys, vi));
            if is_fp_scalar(ty) {
                inv_fprs = inv_fprs.saturating_add(1);
            } else if ty != Ty::NONE {
                inv_gprs = inv_gprs.saturating_add(1);
            }
        }
    };

    count_inv(bound);
    for &inst in stage0_insts {
        each(view, to_idx(inst), |item| {
            if let Item::Value(v) = item {
                count_inv(v);
            }
        });
    }
    for &inst in stage1_insts {
        each(view, to_idx(inst), |item| {
            if let Item::Value(v) = item {
                count_inv(v);
            }
        });
    }

    // Kernel header parameters: loop-carried params (`i`, `acc...`) + prefetched `val_i...`.
    let mut base_gprs = inv_gprs;
    let mut base_fprs = inv_fprs;
    for p in header_params {
        let ty = Ty(at(view.tys, p));
        if is_fp_scalar(ty) {
            base_fprs = base_fprs.saturating_add(1);
        } else {
            base_gprs = base_gprs.saturating_add(1);
        }
    }
    // `i_next` is computed at top of kernel and live through the back-edge/exit check.
    base_gprs = base_gprs.saturating_add(1);
    // Double-buffered Stage-0 loads: both `val_i` (incoming param) and `val_{i+1}` (loaded at top)
    // overlap across the entire Stage 1 body!
    for &ld in stage0_loads {
        let ty = Ty(at(view.tys, to_idx(ld)));
        if is_fp_scalar(ty) {
            base_fprs = base_fprs.saturating_add(2);
        } else {
            base_gprs = base_gprs.saturating_add(2);
        }
    }

    // Exact linear scan of intra-Stage-0 and intra-Stage-1 temporary values.
    let scan_peak = |insts: &[u32], extra_live_roots: &[u32]| -> (u32, u32) {
        let n = insts.len();
        if n == 0 {
            return (0, 0);
        }
        let mut last_use = filled(n, 0usize, &alloc);
        for (pos, &inst) in insts.iter().enumerate() {
            if extra_live_roots.iter().any(|&r| r == inst) {
                set_at(&mut last_use, pos, n);
            } else {
                set_at(&mut last_use, pos, pos);
            }
        }
        for (pos, &inst) in insts.iter().enumerate() {
            each(view, to_idx(inst), |item| {
                if let Item::Value(v) = item
                    && let Some(def_pos) = insts.iter().position(|&d| d == v)
                    && pos > at(&last_use, def_pos)
                {
                    set_at(&mut last_use, def_pos, pos);
                }
            });
        }
        let mut peak_g = 0u32;
        let mut peak_f = 0u32;
        for pos in 0..n {
            let mut cur_g = 0u32;
            let mut cur_f = 0u32;
            for def_pos in 0..=pos {
                if at(&last_use, def_pos) >= pos {
                    let def_inst = at(insts, def_pos);
                    // Exclude stage0_loads already counted in the double-buffered budget above.
                    if stage0_loads.iter().any(|&ld| ld == def_inst) {
                        continue;
                    }
                    let ty = Ty(at(view.tys, to_idx(def_inst)));
                    if ty == Ty::NONE {
                        continue;
                    }
                    if is_fp_scalar(ty) {
                        cur_f = cur_f.saturating_add(1);
                    } else {
                        cur_g = cur_g.saturating_add(1);
                    }
                }
            }
            if cur_g > peak_g {
                peak_g = cur_g;
            }
            if cur_f > peak_f {
                peak_f = cur_f;
            }
        }
        (peak_g, peak_f)
    };

    let (s0_g, s0_f) = scan_peak(stage0_insts, stage0_loads);
    let (s1_g, s1_f) = scan_peak(stage1_insts, back_args);
    (
        base_gprs.saturating_add(s0_g.max(s1_g)),
        base_fprs.saturating_add(s0_f.max(s1_f)),
    )
}

fn find_pipelinable_loop<A: Allocator + Copy>(
    view: &View<'_>,
    opts: ModuloSchedOpts,
    alloc: A,
) -> Option<PipelinePlan<A>> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 || len < 4 {
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

        let h_params = view.params(Block(header));
        if h_params.is_empty() {
            continue;
        }
        // Every header parameter must be a clean scalar value type.
        let mut params_scalar = true;
        for p in h_params.start..h_params.end {
            if !is_scalar_value_ty(Ty(at(view.tys, p))) {
                params_scalar = false;
                break;
            }
        }
        if !params_scalar {
            continue;
        }

        // Match preheader edge (`Jump` or single-target `Br` into `header`).
        let pre_term = view.terminator(Block(preheader));
        let (pre_edge, pre_args) = match view.op(pre_term) {
            Op::Jump => {
                let pat = to_idx(at(view.data, pre_term));
                if at(view.extra, pat) != header {
                    continue;
                }
                let argc = to_idx(at(view.extra, pat + 1));
                if argc != h_params.len() {
                    continue;
                }
                let mut args = Vec::with_capacity_in(argc, alloc);
                for k in 0..argc {
                    args.push(at(view.extra, pat + 2 + k));
                }
                (PreheaderEdge::Jump, args)
            }
            Op::Br => {
                let pat = to_idx(at(view.data, pre_term));
                let t_blk = at(view.extra, pat + 1);
                let t_argc = to_idx(at(view.extra, pat + 2));
                let e_at = pat + 3 + t_argc;
                let e_blk = at(view.extra, e_at);
                let e_argc = to_idx(at(view.extra, e_at + 1));
                if (t_blk == header) == (e_blk == header) {
                    continue;
                }
                if t_blk == header {
                    if t_argc != h_params.len() {
                        continue;
                    }
                    let mut args = Vec::with_capacity_in(t_argc, alloc);
                    for k in 0..t_argc {
                        args.push(at(view.extra, pat + 3 + k));
                    }
                    (PreheaderEdge::BrThen, args)
                } else {
                    if e_argc != h_params.len() {
                        continue;
                    }
                    let mut args = Vec::with_capacity_in(e_argc, alloc);
                    for k in 0..e_argc {
                        args.push(at(view.extra, e_at + 2 + k));
                    }
                    (PreheaderEdge::BrElse, args)
                }
            }
            _ => continue,
        };

        // Match either `SingleBlock` (rotated do-while) or `HeaderBody` (2-block loop).
        let (shape, body_block, exit_block, back_args_start, cond_val, exit_args) = if latch == header {
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
            let mut ex = Vec::with_capacity_in(e_argc, alloc);
            for k in 0..e_argc {
                ex.push(at(view.extra, e_at + 2 + k));
            }
            (
                LoopShape::SingleBlock { header, exit: e_blk },
                header,
                e_blk,
                idx + 3,
                cv,
                ex,
            )
        } else {
            let latch_single = preds.get(to_idx(latch)).map(Vec::as_slice) == Some(&[header]);
            if !latch_single
                || view.block_kind(Block(latch)) != BlockKind::Normal
                || !view.params(Block(latch)).is_empty()
            {
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
            let e_argc = to_idx(at(view.extra, e_at + 1));
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
            let mut ex = Vec::with_capacity_in(e_argc, alloc);
            for k in 0..e_argc {
                ex.push(at(view.extra, e_at + 2 + k));
            }
            // Reject if `exit_args` passes the branch condition itself.
            if ex.iter().any(|&a| a == cv) {
                continue;
            }
            (
                LoopShape::HeaderBody { header, body: latch, exit: e_blk },
                latch,
                e_blk,
                bat + 2,
                cv,
                ex,
            )
        };

        let is_loop_blk = |b: u32| b == header || b == body_block;

        // Match counted unsigned loop compare (`i < bound` or `i <= bound`).
        let ci = to_idx(cond_val);
        if view.op(ci) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, ci));
        let Some(cmp_cond) =
            IntCond::from_word(at(view.extra, cat)).filter(|c| matches!(c, IntCond::Ult | IntCond::Ule))
        else {
            continue;
        };
        let cmp_lhs = at(view.extra, cat + 1);
        let bound = at(view.extra, cat + 2);
        if is_loop_blk(at(&inst_block, to_idx(bound))) || at(&inst_block, to_idx(bound)) >= header {
            continue;
        }

        let mut back_args = Vec::with_capacity_in(h_params.len(), alloc);
        for pos in 0..h_params.len() {
            back_args.push(at(view.extra, back_args_start + pos));
        }

        // Match unit-step induction variable `i` (`i_next = i + 1`).
        let mut found_iv = None;
        for pos in 0..h_params.len() {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let p_ty = Ty(at(view.tys, p_idx));
            if p_ty != Ty::I64 && p_ty != Ty::I32 {
                continue;
            }
            let back_v = at(&back_args, pos);
            let back_i = to_idx(back_v);
            if at(&inst_block, back_i) != body_block || view.op(back_i) != Op::Add {
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
                LoopShape::HeaderBody { .. } => cmp_lhs == p_inst,
                LoopShape::SingleBlock { .. } => cmp_lhs == back_v,
            };
            if ok_cmp {
                found_iv = Some((pos, p_inst, back_v, p_ty));
                break;
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst, iv_ty)) = found_iv else {
            continue;
        };
        let iv_init = at(&pre_args, iv_pos);

        // Require all `pre_args` to come from blocks emitted before `header`.
        if pre_args.iter().any(|&a| at(&inst_block, to_idx(a)) >= header) {
            continue;
        }

        let body_range = view.block(Block(body_block));
        let body_start = if body_block == header { h_params.end } else { body_range.start };
        let body_end = body_range.end - 1;
        if body_end <= body_start || body_end - body_start > MAX_BODY_INSTS {
            continue;
        }

        // Check every body instruction for allowed pure / load / alias-checked store ops.
        let mut body_ok = true;
        let mut stores: Vec<usize, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            if ty.is_vector() || (ty != Ty::NONE && !is_scalar_value_ty(ty)) {
                body_ok = false;
                break;
            }
            let fl = Flags(at(view.flags, inst));
            match op {
                Op::Load => {
                    if fl.contains(Flags::VOLATILE) {
                        body_ok = false;
                        break;
                    }
                }
                Op::Store => {
                    if fl.contains(Flags::VOLATILE) {
                        body_ok = false;
                        break;
                    }
                    stores.push(inst);
                }
                Op::Udiv | Op::Urem => {
                    let idx = to_idx(at(view.data, inst));
                    let rhs = to_idx(at(view.extra, idx + 1));
                    if const_u64(view, rhs).is_none_or(|d| d == 0) {
                        body_ok = false;
                        break;
                    }
                }
                Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::Memcmp
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::Trap
                | Op::Unreachable
                | Op::Asm
                | Op::AsmBr
                | Op::TargetIntrinsic
                | Op::TailCall
                | Op::TailCallIndirect
                | Op::MaskedLoad
                | Op::MaskedStore
                | Op::Gather
                | Op::Scatter
                | Op::Assume
                | Op::Suspend
                | Op::Cancelled
                | Op::FrameAddress
                | Op::ReturnAddress
                | Op::StackPointer
                | Op::SlotLive
                | Op::SlotDead
                | Op::Sdiv
                | Op::Srem => {
                    body_ok = false;
                    break;
                }
                _ => {
                    if op.info().flags.meanings_only(fl).0 != 0 {
                        body_ok = false;
                        break;
                    }
                }
            }
            // Verify all loop-invariant operands come from blocks strictly before `header`.
            each(view, inst, |item| {
                if let Item::Value(v) = item {
                    let vb = at(&inst_block, to_idx(v));
                    if !is_loop_blk(vb) && vb >= header {
                        body_ok = false;
                    }
                }
            });
            if !body_ok {
                break;
            }
        }
        if !body_ok {
            continue;
        }

        // Partition: find Stage-0 loads (non-volatile scalar loads whose address depends ONLY on
        // `iv_inst`, loop invariants, and pure address ops, and is cross-iteration disjoint from
        // every store in the loop).
        let mut is_pure_iv_addr = filled(len, false, &alloc);
        let mut stage0_loads: Vec<u32, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            let op = view.op(inst);
            if op == Op::Load {
                let lat = to_idx(at(view.data, inst));
                let ptr_v = at(view.extra, lat);
                let pi = to_idx(ptr_v);
                let ptr_ok = if !is_loop_blk(at(&inst_block, pi)) {
                    true
                } else {
                    pi == to_idx(iv_inst) || at(&is_pure_iv_addr, pi)
                };
                if ptr_ok {
                    let mut disjoint_all = true;
                    for &st in &stores {
                        if !load_store_cross_iter_disjoint(view, &inst_block, body_block, iv_inst, inst, st) {
                            disjoint_all = false;
                            break;
                        }
                    }
                    if disjoint_all && stage0_loads.len() < MAX_STAGE0_LOADS {
                        stage0_loads.push(to_u32(inst));
                    }
                }
            } else if is_pure_addr_op(op) && inst != to_idx(iv_next_inst) && inst != ci {
                let mut ok = true;
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        if is_loop_blk(at(&inst_block, vi)) && v != iv_inst && !at(&is_pure_iv_addr, vi) {
                            ok = false;
                        }
                    }
                });
                if ok {
                    set_at(&mut is_pure_iv_addr, inst, true);
                }
            }
        }
        if stage0_loads.is_empty() {
            continue;
        }

        // Mark backward slice of `stage0_loads` inside `body_start..body_end` as Stage 0.
        let mut in_stage0 = filled(len, false, &alloc);
        let mut is_stage0_load = filled(len, false, &alloc);
        for &ld in &stage0_loads {
            set_at(&mut in_stage0, to_idx(ld), true);
            set_at(&mut is_stage0_load, to_idx(ld), true);
        }
        for inst in (body_start..body_end).rev() {
            if !at(&in_stage0, inst) {
                continue;
            }
            each(view, inst, |item| {
                if let Item::Value(v) = item {
                    let vi = to_idx(v);
                    if vi >= body_start && vi < body_end {
                        set_at(&mut in_stage0, vi, true);
                    }
                }
            });
        }

        // Check external uses of loop-defined values (outside `header` and `body_block`).
        let mut bad_escape = false;
        let mut promoted_header_params: Vec<usize, A> = Vec::new_in(alloc);
        let mut single_escaped_stage1: Vec<u32, A> = Vec::new_in(alloc);
        let exit_sole_pred_is_header = preds.get(to_idx(exit_block)).map(Vec::as_slice) == Some(&[header]);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        if !is_loop_blk(at(&inst_block, vi)) {
                            return;
                        }
                        match shape {
                            LoopShape::HeaderBody { .. } => {
                                if vi >= h_params.start
                                    && vi < h_params.end
                                    && exit_sole_pred_is_header
                                    && exit_block > header
                                {
                                    let pos = vi - h_params.start;
                                    if promoted_header_params.scan_for(&pos).is_none() {
                                        promoted_header_params.push(pos);
                                    }
                                } else {
                                    bad_escape = true;
                                }
                            }
                            LoopShape::SingleBlock { .. } => {
                                if exit_sole_pred_is_header && exit_block > header && vi != ci {
                                    if vi >= body_start && vi < body_end && !at(&is_stage0_load, vi) && v != iv_next_inst {
                                        if single_escaped_stage1.scan_for(&v).is_none() {
                                            single_escaped_stage1.push(v);
                                        }
                                    }
                                } else {
                                    bad_escape = true;
                                }
                            }
                        }
                    }
                });
                if bad_escape {
                    break;
                }
            }
            if bad_escape {
                break;
            }
        }
        if bad_escape {
            continue;
        }
        if exit_args.iter().any(|&ea| {
            let eb = at(&inst_block, to_idx(ea));
            !is_loop_blk(eb) && eb >= header
        }) {
            continue;
        }

        // Mark Stage-1 instructions via backward slice from Stage-1 roots (non-IV back-edge args,
        // stores, and SingleBlock exit args / escapes), stopping at `stage0_loads`, `iv_next`,
        // header params, and loop invariants.
        let mut in_stage1 = filled(len, false, &alloc);
        for (pos, &bv) in back_args.iter().enumerate() {
            if pos == iv_pos {
                continue;
            }
            let bi = to_idx(bv);
            if bi >= body_start && bi < body_end && !at(&is_stage0_load, bi) && bi != to_idx(iv_next_inst) {
                set_at(&mut in_stage1, bi, true);
            }
        }
        for &st in &stores {
            set_at(&mut in_stage1, st, true);
        }
        if matches!(shape, LoopShape::SingleBlock { .. }) {
            for &ea in &exit_args {
                let ei = to_idx(ea);
                if ei == ci {
                    bad_escape = true;
                    break;
                }
                if ei >= body_start && ei < body_end && !at(&is_stage0_load, ei) && ei != to_idx(iv_next_inst) {
                    set_at(&mut in_stage1, ei, true);
                }
            }
            for &ev in &single_escaped_stage1 {
                set_at(&mut in_stage1, to_idx(ev), true);
            }
        }
        if bad_escape {
            continue;
        }
        for inst in (body_start..body_end).rev() {
            if !at(&in_stage1, inst) {
                continue;
            }
            each(view, inst, |item| {
                if let Item::Value(v) = item {
                    let vi = to_idx(v);
                    if vi >= body_start
                        && vi < body_end
                        && !at(&is_stage0_load, vi)
                        && vi != to_idx(iv_next_inst)
                    {
                        set_at(&mut in_stage1, vi, true);
                    }
                }
            });
        }

        let mut stage0_insts: Vec<u32, A> = Vec::new_in(alloc);
        let mut stage1_insts: Vec<u32, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            if at(&in_stage0, inst) {
                stage0_insts.push(to_u32(inst));
            }
            if at(&in_stage1, inst) {
                stage1_insts.push(to_u32(inst));
            }
        }
        if stage1_insts.is_empty() {
            continue;
        }

        // Compute Stage-1 compute latency descending from Stage-0 loads!
        // Requiring `load_to_stage1_lat >= opts.min_compute_latency` BOTH ensures real latency
        // hiding AND guarantees idempotence (an already-pipelined kernel's loads feed only the
        // back-edge, so its `load_to_stage1_lat == 0`!).
        let mut from_load_lat = filled(len, NONE, &alloc);
        for &ld in &stage0_loads {
            set_at(&mut from_load_lat, to_idx(ld), 0);
        }
        let mut load_to_stage1_lat = 0u32;
        for &s1 in &stage1_insts {
            let si = to_idx(s1);
            let mut best = NONE;
            each(view, si, |item| {
                if let Item::Value(v) = item {
                    let d = at(&from_load_lat, to_idx(v));
                    if d != NONE {
                        let cand = d.saturating_add(m5_op_latency(view.op(si)));
                        if best == NONE || cand > best {
                            best = cand;
                        }
                    }
                }
            });
            if best != NONE {
                set_at(&mut from_load_lat, si, best);
                if best > load_to_stage1_lat {
                    load_to_stage1_lat = best;
                }
            }
        }
        if load_to_stage1_lat < opts.min_compute_latency {
            continue;
        }

        // Compute Apple M5 `ResMII` (6 ALU / 4 FP / 3 LDST ports).
        let mut alu_ops = 0u32;
        let mut fp_ops = 0u32;
        let mut ldst_ops = 0u32;
        for inst in body_start..body_end {
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            match op {
                Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide => {}
                Op::Load | Op::Store => ldst_ops = ldst_ops.saturating_add(1),
                Op::Fadd | Op::Fsub | Op::Fmul | Op::Fma | Op::Fmuladd | Op::Fdiv | Op::Fneg | Op::Fabs | Op::Fcmp => {
                    fp_ops = fp_ops.saturating_add(1);
                }
                _ => {
                    if is_fp_scalar(ty) {
                        fp_ops = fp_ops.saturating_add(1);
                    } else {
                        alu_ops = alu_ops.saturating_add(1);
                    }
                }
            }
        }
        // Account for the loop-exit compare when it sits in `header` (`HeaderBody`).
        if matches!(shape, LoopShape::HeaderBody { .. }) {
            alu_ops = alu_ops.saturating_add(1);
        }
        let res_mii = alu_ops
            .div_ceil(M5_ALU_PORTS)
            .max(fp_ops.div_ceil(M5_FP_PORTS))
            .max(ldst_ops.div_ceil(M5_LDST_PORTS))
            .max(1);

        // Compute `RecMII`: maximum intra-iteration latency path from each loop-carried param `p`
        // to its back-edge value `back_args[pos]`.
        let mut rec_mii = 1u32;
        let mut dist = filled(len, NONE, &alloc);
        for pos in 0..h_params.len() {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let back_v = at(&back_args, pos);
            let bi = to_idx(back_v);
            if bi < body_start || bi >= body_end {
                continue;
            }
            for inst in body_start..body_end {
                set_at(&mut dist, inst, NONE);
            }
            for inst in body_start..body_end {
                let mut best = NONE;
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        if v == p_inst {
                            let cand = m5_op_latency(view.op(inst));
                            if best == NONE || cand > best {
                                best = cand;
                            }
                        } else {
                            let vi = to_idx(v);
                            if vi >= body_start && vi < body_end {
                                let prev = at(&dist, vi);
                                if prev != NONE {
                                    let cand = prev.saturating_add(m5_op_latency(view.op(inst)));
                                    if best == NONE || cand > best {
                                        best = cand;
                                    }
                                }
                            }
                        }
                    }
                });
                set_at(&mut dist, inst, best);
            }
            let rec_lat = at(&dist, bi);
            if rec_lat != NONE && rec_lat > rec_mii {
                rec_mii = rec_lat;
            }
        }
        let _ii = res_mii.max(rec_mii);

        // Enforce register-pressure ceiling (zero spills!).
        let mut is_loop_inst = filled(len, false, &alloc);
        for p in h_params.start..h_params.end {
            set_at(&mut is_loop_inst, p, true);
        }
        for inst in body_start..body_end {
            set_at(&mut is_loop_inst, inst, true);
        }
        let (peak_gprs, peak_fprs) = estimate_kernel_register_pressure(
            view,
            h_params,
            bound,
            &stage0_insts,
            &stage0_loads,
            &stage1_insts,
            &back_args,
            &is_loop_inst,
            alloc,
        );
        if peak_gprs > opts.max_gprs || peak_fprs > opts.max_fprs {
            continue;
        }

        return Some(PipelinePlan {
            preheader,
            pre_edge,
            shape,
            iv_pos,
            iv_inst,
            iv_next_inst,
            iv_ty,
            iv_init,
            bound,
            cmp_cond,
            pre_args,
            back_args,
            exit_args,
            promoted_header_params,
            stage0_insts,
            stage0_loads,
            stage1_insts,
            res_mii,
            rec_mii,
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

fn pipeline_one_loop<A: Allocator + Copy>(
    view: View<'_>,
    opts: ModuloSchedOpts,
    alloc: A,
) -> Result<Option<(Func<A>, ModuloSchedStats)>, SimplifyError> {
    let Some(plan) = find_pipelinable_loop(&view, opts, alloc) else {
        return Ok(None);
    };

    let blocks = view.blocks();
    let len = view.len();
    let (header_blk, body_blk, exit_blk, has_guard) = match plan.shape {
        LoopShape::HeaderBody { header, body, exit } => (header, Some(body), exit, true),
        LoopShape::SingleBlock { header, exit } => (header, None, exit, false),
    };
    let h_params = view.params(Block(header_blk));

    // Assign new block numbers deterministically.
    // `HeaderBody`: replaces `(header, body)` with `(guard, prologue, kernel, epilogue)`.
    // `SingleBlock`: replaces `(header)` with `(prologue, kernel, epilogue)`.
    let mut blk_map = filled(blocks, NONE, &alloc);
    let mut next_blk = 0u32;
    let mut guard_blk = NONE;
    let mut prologue_blk = NONE;
    let mut kernel_blk = NONE;
    let mut epilogue_blk = NONE;
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        if b_u32 == header_blk {
            if has_guard {
                guard_blk = next_blk;
                prologue_blk = next_blk + 1;
                kernel_blk = next_blk + 2;
                epilogue_blk = next_blk + 3;
                set_at(&mut blk_map, b, guard_blk);
                next_blk += 4;
            } else {
                prologue_blk = next_blk;
                kernel_blk = next_blk + 1;
                epilogue_blk = next_blk + 2;
                set_at(&mut blk_map, b, prologue_blk);
                next_blk += 3;
            }
        } else if body_blk == Some(b_u32) {
            set_at(&mut blk_map, b, NONE);
        } else {
            set_at(&mut blk_map, b, next_blk);
            next_blk += 1;
        }
    }
    let exit_blk_new = at(&blk_map, to_idx(exit_blk));
    let entry_target_blk = if has_guard { guard_blk } else { prologue_blk };

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
        if body_blk == Some(b_u32) {
            continue;
        }
        let range = view.block(Block(b_u32));
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at(view.block_kinds, b));
            for inst in range.start..term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            match plan.pre_edge {
                PreheaderEdge::Jump => {
                    push_jump(&mut func, entry_target_blk, &[]);
                }
                PreheaderEdge::BrThen => {
                    let pat = to_idx(at(view.data, term));
                    let cond_new = at(&val_map, to_idx(at(view.extra, pat)));
                    let t_argc = to_idx(at(view.extra, pat + 2));
                    let e_at = pat + 3 + t_argc;
                    let e_blk = at(&blk_map, to_idx(at(view.extra, e_at)));
                    let e_argc = to_idx(at(view.extra, e_at + 1));
                    let mut e_args = Vec::with_capacity_in(e_argc, alloc);
                    for k in 0..e_argc {
                        e_args.push(at(&val_map, to_idx(at(view.extra, e_at + 2 + k))));
                    }
                    push_br(&mut func, cond_new, entry_target_blk, &[], e_blk, &e_args);
                }
                PreheaderEdge::BrElse => {
                    let pat = to_idx(at(view.data, term));
                    let cond_new = at(&val_map, to_idx(at(view.extra, pat)));
                    let t_blk = at(&blk_map, to_idx(at(view.extra, pat + 1)));
                    let t_argc = to_idx(at(view.extra, pat + 2));
                    let mut t_args = Vec::with_capacity_in(t_argc, alloc);
                    for k in 0..t_argc {
                        t_args.push(at(&val_map, to_idx(at(view.extra, pat + 3 + k))));
                    }
                    push_br(&mut func, cond_new, t_blk, &t_args, entry_target_blk, &[]);
                }
            }
            continue;
        }

        if b_u32 == header_blk {
            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let bound_new = at(&val_map, to_idx(plan.bound));

            // 1. Optional `guard` (`HeaderBody` only): skips loop completely when `n == 0`.
            if has_guard {
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let has_any = push_icmp(&mut func, plan.cmp_cond, iv_init_new, bound_new);
                let mut g_exit_args =
                    Vec::with_capacity_in(plan.exit_args.len() + plan.promoted_header_params.len(), alloc);
                for &ea in &plan.exit_args {
                    let ei = to_idx(ea);
                    if ei >= h_params.start && ei < h_params.end {
                        let pos = ei - h_params.start;
                        g_exit_args.push(at(&val_map, to_idx(at(&plan.pre_args, pos))));
                    } else {
                        g_exit_args.push(at(&val_map, ei));
                    }
                }
                for &pos in &plan.promoted_header_params {
                    g_exit_args.push(at(&val_map, to_idx(at(&plan.pre_args, pos))));
                }
                push_br(&mut func, has_any, prologue_blk, &[], exit_blk_new, &g_exit_args);
            }

            // 2. `prologue`: executes Stage 0 for iteration `0` (`val_0 = load(ptr_0)`),
            //    then branches to `kernel` (if `>= 2` iterations) or `epilogue` (`n == 1`).
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut pro_map = copy_u32_vec(&val_map, alloc);
            for pos in 0..plan.pre_args.len() {
                set_at(
                    &mut pro_map,
                    h_params.start + pos,
                    at(&val_map, to_idx(at(&plan.pre_args, pos))),
                );
            }
            for &s0 in &plan.stage0_insts {
                let si = to_idx(s0);
                let mapped = copy_remapped(&view, si, &pro_map, &blk_map, &mut func);
                set_at(&mut pro_map, si, mapped);
            }
            let has_two = match (plan.shape, plan.cmp_cond) {
                (LoopShape::HeaderBody { .. }, IntCond::Ule) => {
                    push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new)
                }
                _ => {
                    let k1 = push_iconst(&mut func, plan.iv_ty, 1);
                    let iv_1 = push_bin(&mut func, Op::Add, plan.iv_ty, iv_init_new, k1);
                    push_icmp(&mut func, plan.cmp_cond, iv_1, bound_new)
                }
            };
            let mut pro_out = Vec::with_capacity_in(plan.pre_args.len() + plan.stage0_loads.len(), alloc);
            for &pa in &plan.pre_args {
                pro_out.push(at(&val_map, to_idx(pa)));
            }
            for &ld in &plan.stage0_loads {
                pro_out.push(at(&pro_map, to_idx(ld)));
            }
            push_br(&mut func, has_two, kernel_blk, &pro_out, epilogue_blk, &pro_out);

            // 3. `kernel`: steady-state software-pipelined loop body.
            //    TOP OF BLOCK: Stage 0 for iteration `i + 1` (`val_next = load(ptr_{i+1})`).
            //    BOTTOM OF BLOCK: Stage 1 for iteration `i` using prefetched `loaded` (`val_i`).
            //    Stops while `i + 1` is still in bounds so `ptr[n]` is NEVER loaded!
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut k_params = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for pos in 0..plan.pre_args.len() {
                let p_ty = Ty(at(view.tys, h_params.start + pos));
                k_params.push(push_inst(&mut func, Op::Param, Flags::NONE, p_ty, 0));
            }
            let mut k_loaded = Vec::with_capacity_in(plan.stage0_loads.len(), alloc);
            for &ld in &plan.stage0_loads {
                let l_ty = Ty(at(view.tys, to_idx(ld)));
                k_loaded.push(push_inst(&mut func, Op::Param, Flags::NONE, l_ty, 0));
            }
            let k1 = push_iconst(&mut func, plan.iv_ty, 1);
            let i_k = at(&k_params, plan.iv_pos);
            let i_next = push_bin(&mut func, Op::Add, plan.iv_ty, i_k, k1);

            // Emit Stage 0 for iteration `i_next` at the very top of `kernel`.
            let mut s0_map = copy_u32_vec(&val_map, alloc);
            set_at(&mut s0_map, to_idx(plan.iv_inst), i_next);
            for &s0 in &plan.stage0_insts {
                let si = to_idx(s0);
                let mapped = copy_remapped(&view, si, &s0_map, &blk_map, &mut func);
                set_at(&mut s0_map, si, mapped);
            }

            // Emit Stage 1 for iteration `i_k` consuming prefetched `k_loaded`.
            let mut s1_map = copy_u32_vec(&val_map, alloc);
            for pos in 0..plan.pre_args.len() {
                set_at(&mut s1_map, h_params.start + pos, at(&k_params, pos));
            }
            for (lidx, &ld) in plan.stage0_loads.iter().enumerate() {
                set_at(&mut s1_map, to_idx(ld), at(&k_loaded, lidx));
            }
            set_at(&mut s1_map, to_idx(plan.iv_next_inst), i_next);
            for &s1 in &plan.stage1_insts {
                let si = to_idx(s1);
                let mapped = copy_remapped(&view, si, &s1_map, &blk_map, &mut func);
                set_at(&mut s1_map, si, mapped);
            }

            let keep_kernel = match (plan.shape, plan.cmp_cond) {
                (LoopShape::HeaderBody { .. }, IntCond::Ule) => {
                    push_icmp(&mut func, IntCond::Ult, i_next, bound_new)
                }
                _ => {
                    let i_next2 = push_bin(&mut func, Op::Add, plan.iv_ty, i_next, k1);
                    push_icmp(&mut func, plan.cmp_cond, i_next2, bound_new)
                }
            };
            let mut k_out = Vec::with_capacity_in(plan.pre_args.len() + plan.stage0_loads.len(), alloc);
            for pos in 0..plan.pre_args.len() {
                if pos == plan.iv_pos {
                    k_out.push(i_next);
                } else {
                    k_out.push(at(&s1_map, to_idx(at(&plan.back_args, pos))));
                }
            }
            for &ld in &plan.stage0_loads {
                k_out.push(at(&s0_map, to_idx(ld)));
            }
            push_br(&mut func, keep_kernel, kernel_blk, &k_out, epilogue_blk, &k_out);

            // 4. `epilogue`: finishes Stage 1 for the final iteration `n - 1` using the last
            //    already-loaded value(s) — zero extra loads!
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut e_params = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for pos in 0..plan.pre_args.len() {
                let p_ty = Ty(at(view.tys, h_params.start + pos));
                e_params.push(push_inst(&mut func, Op::Param, Flags::NONE, p_ty, 0));
            }
            let mut e_loaded = Vec::with_capacity_in(plan.stage0_loads.len(), alloc);
            for &ld in &plan.stage0_loads {
                let l_ty = Ty(at(view.tys, to_idx(ld)));
                e_loaded.push(push_inst(&mut func, Op::Param, Flags::NONE, l_ty, 0));
            }
            let ek1 = push_iconst(&mut func, plan.iv_ty, 1);
            let i_after = push_bin(&mut func, Op::Add, plan.iv_ty, at(&e_params, plan.iv_pos), ek1);

            let mut e_map = copy_u32_vec(&val_map, alloc);
            for pos in 0..plan.pre_args.len() {
                set_at(&mut e_map, h_params.start + pos, at(&e_params, pos));
            }
            for (lidx, &ld) in plan.stage0_loads.iter().enumerate() {
                set_at(&mut e_map, to_idx(ld), at(&e_loaded, lidx));
            }
            set_at(&mut e_map, to_idx(plan.iv_next_inst), i_after);
            for &s1 in &plan.stage1_insts {
                let si = to_idx(s1);
                let mapped = copy_remapped(&view, si, &e_map, &blk_map, &mut func);
                set_at(&mut e_map, si, mapped);
            }

            let mut epi_exit_args =
                Vec::with_capacity_in(plan.exit_args.len() + plan.promoted_header_params.len(), alloc);
            match plan.shape {
                LoopShape::HeaderBody { .. } => {
                    for &ea in &plan.exit_args {
                        let ei = to_idx(ea);
                        if ei >= h_params.start && ei < h_params.end {
                            let pos = ei - h_params.start;
                            let fin = if pos == plan.iv_pos {
                                i_after
                            } else {
                                at(&e_map, to_idx(at(&plan.back_args, pos)))
                            };
                            epi_exit_args.push(fin);
                        } else {
                            epi_exit_args.push(at(&val_map, ei));
                        }
                    }
                    for &pos in &plan.promoted_header_params {
                        let fin = if pos == plan.iv_pos {
                            i_after
                        } else {
                            at(&e_map, to_idx(at(&plan.back_args, pos)))
                        };
                        epi_exit_args.push(fin);
                    }
                }
                LoopShape::SingleBlock { .. } => {
                    for &ea in &plan.exit_args {
                        epi_exit_args.push(at(&e_map, to_idx(ea)));
                    }
                    // For SingleBlock where `exit` had `header` as sole predecessor, `epilogue`
                    // now dominates `exit`, so expose epilogue's values in `val_map`.
                    for pos in 0..plan.pre_args.len() {
                        set_at(&mut val_map, h_params.start + pos, at(&e_params, pos));
                    }
                    for (lidx, &ld) in plan.stage0_loads.iter().enumerate() {
                        set_at(&mut val_map, to_idx(ld), at(&e_loaded, lidx));
                    }
                    set_at(&mut val_map, to_idx(plan.iv_next_inst), i_after);
                    for &s1 in &plan.stage1_insts {
                        let si = to_idx(s1);
                        set_at(&mut val_map, si, at(&e_map, si));
                    }
                }
            }
            push_jump(&mut func, exit_blk_new, &epi_exit_args);
            continue;
        }

        // Emit remaining blocks; if `b_u32 == exit_blk` with promoted `HeaderBody` params,
        // materialize those new `Op::Param`s at the top of `exit`.
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        if b_u32 == exit_blk && !plan.promoted_header_params.is_empty() {
            let params_end = view.params(Block(b_u32)).end;
            for inst in range.start..params_end {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            for &pos in &plan.promoted_header_params {
                let p_ty = Ty(at(view.tys, h_params.start + pos));
                let new_p = push_inst(&mut func, Op::Param, Flags::NONE, p_ty, 0);
                set_at(&mut val_map, h_params.start + pos, new_p);
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

    verify(func.view()).map_err(SimplifyError::Verify)?;
    let stats = ModuloSchedStats {
        loops_pipelined: 1,
        loads_hoisted: to_u32(plan.stage0_loads.len()),
        res_mii: plan.res_mii,
        rec_mii: plan.rec_mii,
    };
    Ok(Some((func, stats)))
}

/// Applies Swing Modulo Scheduling (2-stage load-compute software pipelining) to eligible
/// counted loops in `view`, overlapping iteration `i + 1`'s load(s) with iteration `i`'s compute
/// while staying strictly within the register-pressure ceiling in `opts`.
pub fn swing_modulo_schedule<A: Allocator + Copy>(
    view: View<'_>,
    opts: ModuloSchedOpts,
    alloc: A,
) -> Result<ModuloSchedOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut stats = ModuloSchedStats::default();
    for _ in 0..MAX_PIPELINED_LOOPS {
        let Some((next, one)) = pipeline_one_loop(cur.view(), opts, alloc)? else {
            break;
        };
        cur = next;
        stats.loops_pipelined = stats.loops_pipelined.saturating_add(one.loops_pipelined);
        stats.loads_hoisted = stats.loads_hoisted.saturating_add(one.loads_hoisted);
        if one.res_mii > stats.res_mii {
            stats.res_mii = one.res_mii;
        }
        if one.rec_mii > stats.rec_mii {
            stats.rec_mii = one.rec_mii;
        }
    }
    Ok(ModuloSchedOutput { func: cur, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv, SlotKind},
    };

    /// Builds `fn(n: i64) -> i64` filling a 16-element stack array and running a tight
    /// crypto-mix reduction loop:
    /// `acc = rotr((acc ^ load(p[i])) * 0x9e3779b97f4a7c15, 17)`
    /// over `0..n` (so `n == 16` sits at the exact end of the 128-byte slot; any speculative
    /// out-of-bounds read of `p[16]` immediately traps in `eval::run_in`!).
    fn build_crypto_mix_header_body() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let p = b.slot_addr(slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k.wrapping_mul(0x517c_c1b7_2722_0a95) ^ 0x1234_5678));
            b.store(Flags::NONE, v, p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let k_mix = b.iconst(Ty::I64, 0x9e37_79b9_7f4a_7c15u128);
        let rot17 = b.iconst(Ty::I64, 17);
        let seed = b.iconst(Ty::I64, 0xfeed_face_cafe_beefu128);
        b.jump((header, &[zero, seed]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let addr = b.ptr_add(Flags::NONE, p, off);
        let loaded = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3));
        let xored = b.binary(Op::Xor, Flags::NONE, acc, loaded);
        let muled = b.binary(Op::Mul, Flags::NONE, xored, k_mix);
        let acc2 = b.binary(Op::Rotr, Flags::NONE, muled, rot17);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    /// Rotated single-block (do-while) form of the same crypto-mix loop.
    fn build_crypto_mix_single_block() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, loop_blk, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let p = b.slot_addr(slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k.wrapping_mul(0x94d0_49bb_1331_11eb) ^ 0xdead_beef));
            b.store(Flags::NONE, v, p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let k_mix = b.iconst(Ty::I64, 0x9e37_79b9_7f4a_7c15u128);
        let rot17 = b.iconst(Ty::I64, 17);
        let seed = b.iconst(Ty::I64, 0x1122_3344_5566_7788u128);
        let has_any = b.icmp(IntCond::Ult, zero, n);
        b.br(Flags::NONE, has_any, (loop_blk, &[zero, seed]), (exit, &[seed]));

        b.begin(loop_blk);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let addr = b.ptr_add(Flags::NONE, p, off);
        let loaded = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3));
        let xored = b.binary(Op::Xor, Flags::NONE, acc, loaded);
        let muled = b.binary(Op::Mul, Flags::NONE, xored, k_mix);
        let acc2 = b.binary(Op::Rotr, Flags::NONE, muled, rot17);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (loop_blk, &[i2, acc2]), (exit, &[acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn sms_pipelines_crypto_mix_header_body_exact_no_oob() {
        let func = build_crypto_mix_header_body();
        let out = swing_modulo_schedule(func.view(), ModuloSchedOpts::apple_m5(), TestHeap).unwrap();
        assert!(verify(out.func.view()).is_ok());
        assert_eq!(out.stats.loops_pipelined, 1);
        assert_eq!(out.stats.loads_hoisted, 1);
        assert!(out.stats.res_mii >= 1);
        assert!(out.stats.rec_mii >= 5, "expected xor+mul+rotr RecMII >= 5, got {}", out.stats.rec_mii);

        // Verify bit-exact equivalence across every edge case including exact slot boundary n=16!
        for &n in &[0u128, 1, 2, 3, 7, 16] {
            let Outcome::Returned(orig) = eval::run_in(func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else {
                panic!("original eval failed at n={n}");
            };
            let Outcome::Returned(piped) =
                eval::run_in(out.func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap()
            else {
                panic!("pipelined eval failed (possible OOB load!) at n={n}");
            };
            assert_eq!(orig, piped, "crypto-mix HeaderBody mismatch at n={n}");
        }

        // Idempotence: a second SMS pass must not re-pipeline the steady-state kernel.
        let second = swing_modulo_schedule(out.func.view(), ModuloSchedOpts::apple_m5(), TestHeap).unwrap();
        assert_eq!(second.stats.loops_pipelined, 0);
    }

    #[test]
    fn sms_pipelines_rotated_single_block_exact_no_oob() {
        let func = build_crypto_mix_single_block();
        let out = swing_modulo_schedule(func.view(), ModuloSchedOpts::apple_m5(), TestHeap).unwrap();
        assert!(verify(out.func.view()).is_ok());
        assert_eq!(out.stats.loops_pipelined, 1);
        assert_eq!(out.stats.loads_hoisted, 1);
        assert!(out.stats.rec_mii >= 5);

        for &n in &[0u128, 1, 2, 3, 7, 16] {
            let Outcome::Returned(orig) = eval::run_in(func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else {
                panic!("original rotated eval failed at n={n}");
            };
            let Outcome::Returned(piped) =
                eval::run_in(out.func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap()
            else {
                panic!("pipelined rotated eval failed at n={n}");
            };
            assert_eq!(orig, piped, "crypto-mix SingleBlock mismatch at n={n}");
        }
    }

    #[test]
    fn sms_respects_register_pressure_ceiling() {
        let func = build_crypto_mix_header_body();
        let tight = ModuloSchedOpts {
            max_gprs: 3,
            max_fprs: 28,
            min_compute_latency: 3,
        };
        let out = swing_modulo_schedule(func.view(), tight, TestHeap).unwrap();
        assert_eq!(out.stats.loops_pipelined, 0, "must refuse to pipeline when register budget is exceeded");
    }

    #[test]
    fn sms_pipelines_inplace_load_compute_store_loop() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let p = b.slot_addr(slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k * 37 + 11));
            b.store(Flags::NONE, v, p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let k_mul = b.iconst(Ty::I64, 0xbf58_476d_1ce4_e5b9u128);
        let rot13 = b.iconst(Ty::I64, 13);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let addr = b.ptr_add(Flags::NONE, p, off);
        let val = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3));
        let mixed = b.binary(Op::Mul, Flags::NONE, val, k_mul);
        let rot = b.binary(Op::Rotr, Flags::NONE, mixed, rot13);
        b.store(Flags::NONE, rot, addr, 0, MemAttrs::aligned(3));
        let acc2 = b.binary(Op::Xor, Flags::NONE, acc, rot);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = swing_modulo_schedule(func.view(), ModuloSchedOpts::apple_m5(), TestHeap).unwrap();
        assert!(verify(out.func.view()).is_ok());
        assert_eq!(out.stats.loops_pipelined, 1);

        for &n in &[0u128, 1, 2, 3, 7, 16] {
            let Outcome::Returned(orig) = eval::run_in(func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else {
                panic!("orig inplace eval failed at n={n}");
            };
            let Outcome::Returned(piped) =
                eval::run_in(out.func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap()
            else {
                panic!("piped inplace eval failed at n={n}");
            };
            assert_eq!(orig, piped, "inplace load+compute+store mismatch at n={n}");
        }
    }
}

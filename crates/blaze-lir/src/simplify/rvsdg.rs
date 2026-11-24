//! Whole-Region E-Graph / RVSDG Co-Optimizer with Microarchitectural Port-Throughput Cost Model
//! (`blaze_lir::rvsdg`).
//!
//! Solves the classical compiler phase-ordering problem by evaluating competing multi-pass
//! co-optimization trajectories over structured control-flow regions and selecting the
//! strictly lowest-cost verified candidate under an explicit target microarchitecture model
//! ([`PortCostModel`]):
//!
//! - **Strategy A** (`egraph -> bce -> ifconv -> loopnest -> vectorize -> egraph`):
//!   algebraic simplification first, unlocking branchless masked SIMD & loop-nest vectorization.
//! - **Strategy B** (`loopnest -> egraph -> bce -> ifconv -> slp -> unroll -> egraph`):
//!   loop fusion/interchange first, unlocking cross-loop algebraic cancellation, SLP, and
//!   multi-accumulator scalar unrolling.
//! - **Strategy C** (`egraph -> bce -> unswitch -> egraph -> vectorize/memchr`):
//!   loop unswitching first, hoisting loop-invariant branches out of hot loops to expose SIMD
//!   and `memchr` idioms.
//!
//! Pure sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit
//! allocator (`blaze_own::Vec<T, A>`) only.

use core::alloc::Allocator;

use super::{
    SimplifyError,
    bce::eliminate_redundant_checks,
    dce::{Item, coalesce_cfg, dead, each},
    egraph::{aegraph_optimize, hoist_loop_invariants, materialize_strength_reduction},
    ifconv::if_convert,
    loopnest::{fuse_adjacent_loops, interchange_and_tile_loops},
    propagate,
    slp::{slp_vectorize, vectorize_memchr_loops},
    unroll::unroll_scalar_loops,
    unswitch::unswitch_loops,
    vectorize::auto_vectorize_loops_with_width,
};
use crate::{
    Block, BlockKind, Func, Op, Ty, View,
    arith::{iconst_value, wide_value},
    verify,
};

/// Stack-tracked block limit for zero-allocation exact natural-loop & probability analysis.
const MAX_TRACKED_BLOCKS: usize = 512;
/// Fixed-point unit execution probability (`1.0 == 1024`).
const BASE_PROB: u32 = 1024;
/// Fixed-point cost scale (`1 cycle == 100` cost units).
const CYC_SCALE: u64 = 100;

/// Target microarchitecture execution-port throughput & latency cost model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PortCostModel {
    /// Peak simple integer ALU throughput per cycle, scaled by 10 (e.g., `80` = 8-wide ALU).
    pub alu_throughput_x10: u32,
    /// Integer/FP multiply execution latency/port cost in cycles.
    pub mul_cost: u32,
    /// Integer/FP division & remainder cost in cycles.
    pub div_cost: u32,
    /// L1 cache hit load cost in cycles.
    pub load_cost: u32,
    /// Unpredictable interior branch misprediction penalty in cycles.
    pub branch_cost: u32,
    /// Native SIMD vector register width in bytes (`16` = NEON/SSE2, `32` = AVX2, `64` = AVX-512).
    pub vec_width_bytes: u32,
    /// Whether single-instruction fused multiply-add (`fmadd`/`fmla`/`vfmadd`) is available.
    pub has_fma: bool,
    /// Whether ARMv8.2-SHA3 3-input bitwise instructions (`eor3`, `bcax`) are available.
    pub has_sha3: bool,
}

impl PortCostModel {
    /// Apple Silicon (M-series P-core): 8-wide ALU, 2-cycle mul, 14-cycle div, 4-cycle L1 load,
    /// 16-byte NEON, FMA + SHA3 (`eor3`/`bcax`).
    pub const fn apple_silicon() -> Self {
        Self {
            alu_throughput_x10: 80,
            mul_cost: 2,
            div_cost: 14,
            load_cost: 4,
            branch_cost: 15,
            vec_width_bytes: 16,
            has_fma: true,
            has_sha3: true,
        }
    }

    /// AMD Zen 5 (`x86_64`): 6-wide ALU, 3-cycle mul, 16-cycle div, 4-cycle L1 load,
    /// 32-byte AVX2 (64-byte AVX-512 capable), FMA.
    pub const fn x86_64_zen5() -> Self {
        Self {
            alu_throughput_x10: 60,
            mul_cost: 3,
            div_cost: 16,
            load_cost: 4,
            branch_cost: 15,
            vec_width_bytes: 32,
            has_fma: true,
            has_sha3: false,
        }
    }
}

/// Estimated execution-port cost breakdown of a LIR function under a [`PortCostModel`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct RegionCost {
    /// Total loop-depth-weighted and probability-weighted execution-port cost across all blocks.
    pub total_cost: u64,
    /// Portion of [`RegionCost::total_cost`] incurred inside loops (`depth >= 1`).
    pub loop_body_cost: u64,
    /// Total number of LIR instructions in the function.
    pub inst_count: u32,
}

/// Telemetry returned by [`co_optimize_regions`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RvsdgStats {
    /// Number of co-optimization trajectory rounds evaluated.
    pub rounds: u32,
    /// Baseline cost of the input function under the target [`PortCostModel`].
    pub initial_cost: u64,
    /// Final cost of the winning verified candidate.
    pub final_cost: u64,
    /// Name of the winning optimization trajectory (`"baseline"`, `"strategy_a_egraph_simd"`,
    /// `"strategy_b_loopnest_unroll"`, or `"strategy_c_unswitch_simd"`).
    pub chosen_strategy: &'static str,
}

/// Output of [`co_optimize_regions`].
#[derive(Debug)]
pub struct RvsdgOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: RvsdgStats,
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
fn at_u32(s: &[u32], i: usize) -> u32 { s.get(i).copied().unwrap_or(0) }
#[inline]
fn at_u16(s: &[u16], i: usize) -> u16 { s.get(i).copied().unwrap_or(0) }

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn add_prob(prob: &mut [u32], idx: usize, add: u32) {
    if let Some(slot) = prob.get_mut(idx) {
        *slot = (*slot + add).min(BASE_PROB);
    }
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() || ty.bits(64) > 64 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, width),
        Op::IconstWide => {
            let at = to_idx(view.data.get(inst).copied()?);
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    u64::try_from(v).ok()
}

/// Returns `true` if block `from` has a terminator edge targeting `target`.
#[inline]
fn has_edge(view: &View<'_>, from: usize, target: u32) -> bool {
    let mut found = false;
    let term = view.terminator(Block(to_u32(from)));
    each(view, term, |item| {
        if let Item::Edge(edge) = item {
            if edge.block == target {
                found = true;
            }
        }
    });
    found
}

/// Computes the constant induction-variable step `K >= 1` for a natural loop header `h`
/// (e.g., `K = 4 * lanes` for 4x-unrolled SIMD, `K = 4` for 4x scalar unroll, `K = 16` for
/// SIMD `memchr`, `K = 1` for a standard scalar loop).
fn detect_loop_iv_step(view: &View<'_>, header: usize, in_loop: &[bool], n_tracked: usize) -> u32 {
    let header_u32 = to_u32(header);
    let h_params = view.params(Block(header_u32));
    if h_params.is_empty() {
        return 1;
    }
    let mut max_step = 1u32;
    for l in header..n_tracked {
        if !at_bool(in_loop, l) {
            continue;
        }
        let term = view.terminator(Block(to_u32(l)));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                if edge.block == header_u32 {
                    if let Some((start, argc)) = edge.args {
                        if argc == h_params.len() {
                            for pos in 0..argc {
                                let p_inst = to_u32(h_params.start + pos);
                                let back_v = to_idx(at_u32(view.extra, start + pos));
                                if view.op(back_v) == Op::Add {
                                    let at = to_idx(at_u32(view.data, back_v));
                                    let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                                    let step = if a == p_inst {
                                        const_u64(view, to_idx(b))
                                    } else if b == p_inst {
                                        const_u64(view, to_idx(a))
                                    } else {
                                        None
                                    };
                                    if let Some(s) = step {
                                        if (1..=64).contains(&s) {
                                            max_step = max_step.max(u32::try_from(s).unwrap_or(1));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }
    max_step
}

/// Checks if instruction `inst` is a fused FMA candidate (`Fadd`/`Fsub` consuming a same-block `Fmul`).
#[inline]
fn is_fma_fused(view: &View<'_>, inst: usize, block_start: usize, model: PortCostModel) -> bool {
    if !model.has_fma {
        return false;
    }
    let op = view.op(inst);
    if !matches!(op, Op::Fadd | Op::Fsub) {
        return false;
    }
    let at = to_idx(at_u32(view.data, inst));
    let (l, r) = (to_idx(at_u32(view.extra, at)), to_idx(at_u32(view.extra, at + 1)));
    (l >= block_start && l < inst && view.op(l) == Op::Fmul)
        || (r >= block_start && r < inst && view.op(r) == Op::Fmul)
}

/// Checks if instruction `inst` is a fused SHA3 3-input bitwise op (`eor3` or `bcax`) in the same block.
#[inline]
fn is_sha3_fused(view: &View<'_>, inst: usize, block_start: usize, model: PortCostModel) -> bool {
    if !model.has_sha3 || view.op(inst) != Op::Xor {
        return false;
    }
    let at = to_idx(at_u32(view.data, inst));
    let (l, r) = (to_idx(at_u32(view.extra, at)), to_idx(at_u32(view.extra, at + 1)));
    // `eor3`: `Xor(Xor(a, b), c)` or `Xor(a, Xor(b, c))`
    if (l >= block_start && l < inst && view.op(l) == Op::Xor)
        || (r >= block_start && r < inst && view.op(r) == Op::Xor)
    {
        return true;
    }
    // `bcax`: `Xor(a, And(Not(b), c))` or `Xor(a, And(b, Not(c)))`
    let is_and_not = |cand: usize| -> bool {
        if cand < block_start || cand >= inst || view.op(cand) != Op::And {
            return false;
        }
        let cat = to_idx(at_u32(view.data, cand));
        let (x, y) = (to_idx(at_u32(view.extra, cat)), to_idx(at_u32(view.extra, cat + 1)));
        (x >= block_start && x < cand && view.op(x) == Op::Not)
            || (y >= block_start && y < cand && view.op(y) == Op::Not)
    };
    is_and_not(l) || is_and_not(r)
}

/// Estimates the single-execution microarchitectural port cost of instruction `inst` (in
/// centi-cycles, where `100 == 1.0 cycle`).
fn inst_port_cost(
    view: &View<'_>,
    inst: usize,
    blk: usize,
    block_start: usize,
    is_loop_header: bool,
    model: PortCostModel,
) -> u64 {
    let op = view.op(inst);
    let ty = Ty(at_u16(view.tys, inst));
    let alu_unit = (1000 / u64::from(model.alu_throughput_x10.max(1))).max(10);

    // Determine effective hardware vector register width multiplier for vector operations.
    let op_ty = if op == Op::Store || op == Op::MaskedStore || op == Op::Scatter {
        let sv = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
        Ty(at_u16(view.tys, sv))
    } else {
        ty
    };
    let hw_vec_ops = if op_ty.is_vector() {
        let vec_bytes = (op_ty.elem().bits(64) * op_ty.lanes()).div_ceil(8);
        u64::from(vec_bytes.div_ceil(model.vec_width_bytes.max(16)).max(1))
    } else {
        1
    };

    match op {
        Op::Param | Op::Jump | Op::SlotLive | Op::SlotDead | Op::Assume | Op::Unreachable => 0,

        Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide | Op::Vconst | Op::Null | Op::Uninit => 5,

        Op::Return | Op::Resume | Op::Trap | Op::Cancelled => alu_unit,

        Op::Add
        | Op::Sub
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Not
        | Op::Shl
        | Op::Lshr
        | Op::Ashr
        | Op::Rotl
        | Op::Rotr
        | Op::UaddSat
        | Op::SaddSat
        | Op::UsubSat
        | Op::SsubSat
        | Op::Umin
        | Op::Umax
        | Op::Smin
        | Op::Smax
        | Op::UaddOverflow
        | Op::SaddOverflow
        | Op::UsubOverflow
        | Op::SsubOverflow
        | Op::Clz
        | Op::Ctz
        | Op::Popcnt
        | Op::Bswap
        | Op::Bitrev
        | Op::Icmp
        | Op::Select
        | Op::Zext
        | Op::Sext
        | Op::Trunc
        | Op::Bitcast
        | Op::Freeze
        | Op::PtrAdd
        | Op::PtrAddr
        | Op::Expose
        | Op::FromExposed
        | Op::PtrWithAddr
        | Op::FuncAddr
        | Op::DataAddr
        | Op::TlsAddr
        | Op::SlotAddr
        | Op::Project
        | Op::FrameAddress
        | Op::ReturnAddress
        | Op::StackPointer => alu_unit * hw_vec_ops,

        Op::Xor => {
            if is_sha3_fused(view, inst, block_start, model) {
                0
            } else {
                alu_unit * hw_vec_ops
            }
        }

        Op::Mul | Op::Umulhi | Op::Smulhi | Op::UmulOverflow | Op::SmulOverflow | Op::Fmul => {
            u64::from(model.mul_cost) * CYC_SCALE * hw_vec_ops
        }

        Op::Fadd | Op::Fsub => {
            if is_fma_fused(view, inst, block_start, model) {
                alu_unit * hw_vec_ops
            } else {
                u64::from(model.mul_cost) * 75 * hw_vec_ops
            }
        }

        Op::Fma | Op::Fmuladd => {
            if model.has_fma {
                u64::from(model.mul_cost) * CYC_SCALE * hw_vec_ops
            } else {
                (u64::from(model.mul_cost) * CYC_SCALE + 150) * hw_vec_ops
            }
        }

        Op::Fcmp
        | Op::Fneg
        | Op::Fabs
        | Op::FminNum
        | Op::FmaxNum
        | Op::Fminimum
        | Op::Fmaximum
        | Op::Fcopysign
        | Op::Floor
        | Op::Ceil
        | Op::Ftrunc
        | Op::Nearest
        | Op::Fround
        | Op::Fpext
        | Op::Fptrunc
        | Op::FptoUi
        | Op::FptoSi
        | Op::UitoFp
        | Op::SitoFp => u64::from(model.mul_cost) * 75 * hw_vec_ops,

        Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem | Op::Fdiv | Op::Frem | Op::Sqrt => {
            u64::from(model.div_cost) * CYC_SCALE * hw_vec_ops
        }

        Op::Load | Op::MaskedLoad => u64::from(model.load_cost) * CYC_SCALE * hw_vec_ops,

        Op::Store | Op::MaskedStore => u64::from(model.load_cost) * 80 * hw_vec_ops,

        Op::Gather | Op::Scatter => {
            u64::from(model.load_cost) * CYC_SCALE * u64::from(op_ty.lanes().max(1))
        }

        Op::AtomicLoad | Op::AtomicStore | Op::AtomicRmw | Op::Cmpxchg | Op::Fence => {
            u64::from(model.load_cost) * 200
        }

        Op::Memcpy | Op::Memmove | Op::Memset | Op::Memcmp => u64::from(model.load_cost) * 300,

        Op::Splat | Op::Extract | Op::Insert | Op::Shuffle | Op::Swizzle | Op::Bitmask => {
            alu_unit * 2 * hw_vec_ops
        }

        Op::ReduceAdd
        | Op::ReduceMul
        | Op::ReduceAnd
        | Op::ReduceOr
        | Op::ReduceXor
        | Op::ReduceUmin
        | Op::ReduceUmax
        | Op::ReduceSmin
        | Op::ReduceSmax
        | Op::ReduceFadd
        | Op::ReduceFmul
        | Op::ReduceFmin
        | Op::ReduceFmax => u64::from(model.mul_cost) * CYC_SCALE * hw_vec_ops,

        Op::Br => {
            // Predictable loop control branch (header check or latch backedge) pays 20% of
            // mispredict penalty; interior data-dependent conditional branches pay full penalty.
            let mut has_back = false;
            each(view, inst, |item| {
                if let Item::Edge(edge) = item {
                    if to_idx(edge.block) <= blk {
                        has_back = true;
                    }
                }
            });
            if is_loop_header || has_back {
                (u64::from(model.branch_cost) * CYC_SCALE) / 5
            } else {
                u64::from(model.branch_cost) * CYC_SCALE
            }
        }

        Op::Switch => u64::from(model.branch_cost) * 150,

        Op::Call
        | Op::Invoke
        | Op::TailCall
        | Op::TargetIntrinsic
        | Op::Asm
        | Op::AsmBr
        | Op::Suspend => u64::from(model.branch_cost) * CYC_SCALE + 500,

        Op::CallIndirect | Op::InvokeIndirect | Op::TailCallIndirect => {
            u64::from(model.branch_cost) * CYC_SCALE + 1000
        }
    }
}

/// Deterministically estimates the loop-depth-weighted microarchitectural port-throughput cost
/// of `view` under `model` (zero heap allocation, pure sans-IO).
///
/// Loop depth weighting: `1x` outside loops, `16x` at depth 1, `256x` at depth 2 (`16^d` up to
/// depth 5), amortized by loop induction-variable step (`K = 4*lanes` for 4x-unrolled SIMD,
/// `K = 4` for 4x scalar unrolling) and forward branch execution probabilities.
pub fn estimate_function_cost(view: View<'_>, model: PortCostModel) -> RegionCost {
    let blocks = view.blocks();
    if blocks == 0 {
        return RegionCost::default();
    }
    let n_tracked = blocks.min(MAX_TRACKED_BLOCKS);

    let mut depth = [0u8; MAX_TRACKED_BLOCKS];
    let mut has_backedge = [false; MAX_TRACKED_BLOCKS];
    let mut loop_step = [1u32; MAX_TRACKED_BLOCKS];
    let mut is_tail_epilogue = [false; MAX_TRACKED_BLOCKS];
    let mut ends_in_cold_trap = [false; MAX_TRACKED_BLOCKS];
    let mut prob = [0u32; MAX_TRACKED_BLOCKS];

    // 1. Identify backedge targets (loop headers) and cold/trap blocks.
    for b in 0..n_tracked {
        let b32 = to_u32(b);
        let term = view.terminator(Block(b32));
        let top = view.op(term);
        if view.block_kind(Block(b32)) == BlockKind::Cold
            || matches!(top, Op::Trap | Op::Unreachable | Op::Resume)
        {
            set_at(&mut ends_in_cold_trap, b, true);
        }
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if tb <= b && tb < n_tracked {
                    set_at(&mut has_backedge, tb, true);
                }
            }
        });
    }

    // 2. Compute exact natural-loop nesting depth & IV step per block.
    for h in 0..n_tracked {
        if !at_bool(&has_backedge, h) {
            continue;
        }
        let h32 = to_u32(h);
        let mut in_loop = [false; MAX_TRACKED_BLOCKS];
        let mut work = [0u32; MAX_TRACKED_BLOCKS];
        let mut work_len = 0usize;
        set_at(&mut in_loop, h, true);
        for l in h..n_tracked {
            if has_edge(&view, l, h32) && !at_bool(&in_loop, l) {
                set_at(&mut in_loop, l, true);
                set_at(&mut work, work_len, to_u32(l));
                work_len += 1;
            }
        }
        while work_len > 0 {
            work_len -= 1;
            let cur = at_u32(&work, work_len);
            for p in h..n_tracked {
                if !at_bool(&in_loop, p) && has_edge(&view, p, cur) {
                    set_at(&mut in_loop, p, true);
                    set_at(&mut work, work_len, to_u32(p));
                    work_len += 1;
                }
            }
        }

        let step_k = detect_loop_iv_step(&view, h, &in_loop, n_tracked);
        for b in h..n_tracked {
            if at_bool(&in_loop, b) {
                let next_d = at_u8(&depth, b).saturating_add(1);
                set_at(&mut depth, b, next_d);
                if step_k > at_u32(&loop_step, b) {
                    set_at(&mut loop_step, b, step_k);
                }
            }
        }
    }

    // 3. Detect scalar remainder/epilogue loops immediately following a fast-path step-`K > 1` loop
    //    (e.g., `vec_loop -> vec_reduce -> scalar_tail`, `unroll_loop -> unroll_reduce -> scalar_tail`,
    //    or `memchr_simd -> scalar_tail`).
    for h2 in 1..n_tracked {
        if !at_bool(&has_backedge, h2) || at_u32(&loop_step, h2) > 1 {
            continue;
        }
        let h2_u32 = to_u32(h2);
        let mut fed_by_fast_loop = false;
        for b in 0..h2 {
            if at_u32(&loop_step, b) <= 1 {
                continue;
            }
            // Direct edge (1 hop) from fast-path loop to `h2`.
            if has_edge(&view, b, h2_u32) {
                fed_by_fast_loop = true;
                break;
            }
            // 2-hop edge (`fast_loop -> reduce_block -> h2`) where intermediate block is outside loops.
            let term = view.terminator(Block(to_u32(b)));
            each(&view, term, |item| {
                if let Item::Edge(edge) = item {
                    let mid = to_idx(edge.block);
                    if mid > b && mid < h2 && at_u8(&depth, mid) == 0 && has_edge(&view, mid, h2_u32) {
                        fed_by_fast_loop = true;
                    }
                }
            });
            if fed_by_fast_loop {
                break;
            }
        }
        if fed_by_fast_loop {
            // Mark every block in `h2`'s natural loop as a scalar tail epilogue.
            let mut in_tail = [false; MAX_TRACKED_BLOCKS];
            let mut work = [0u32; MAX_TRACKED_BLOCKS];
            let mut work_len = 0usize;
            set_at(&mut in_tail, h2, true);
            for l in h2..n_tracked {
                if has_edge(&view, l, h2_u32) && !at_bool(&in_tail, l) {
                    set_at(&mut in_tail, l, true);
                    set_at(&mut work, work_len, to_u32(l));
                    work_len += 1;
                }
            }
            while work_len > 0 {
                work_len -= 1;
                let cur = at_u32(&work, work_len);
                for p in h2..n_tracked {
                    if !at_bool(&in_tail, p) && has_edge(&view, p, cur) {
                        set_at(&mut in_tail, p, true);
                        set_at(&mut work, work_len, to_u32(p));
                        work_len += 1;
                    }
                }
            }
            for b in h2..n_tracked {
                if at_bool(&in_tail, b) {
                    set_at(&mut is_tail_epilogue, b, true);
                }
            }
        }
    }

    // 4. Forward block execution probability propagation (fixed-point `BASE_PROB = 1024`).
    prob[0] = BASE_PROB;
    for b in 0..n_tracked {
        let pb = at_u32(&prob, b);
        if pb == 0 {
            continue;
        }
        let term = view.terminator(Block(to_u32(b)));
        let top = view.op(term);
        match top {
            Op::Jump => {
                let t = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, term))));
                if t > b && t < n_tracked {
                    add_prob(&mut prob, t, pb);
                }
            }
            Op::Br => {
                let at = to_idx(at_u32(view.data, term));
                let t_blk = to_idx(at_u32(view.extra, at + 1));
                let t_argc = to_idx(at_u32(view.extra, at + 2));
                let e_blk = to_idx(at_u32(view.extra, at + 3 + t_argc));
                let t_fwd = t_blk > b && t_blk < n_tracked;
                let e_fwd = e_blk > b && e_blk < n_tracked;

                if t_fwd && !e_fwd {
                    add_prob(&mut prob, t_blk, pb);
                } else if e_fwd && !t_fwd {
                    add_prob(&mut prob, e_blk, pb);
                } else if t_fwd && e_fwd {
                    if at_bool(&has_backedge, b) {
                        // Loop header: enters loop body and eventually exits with full probability.
                        add_prob(&mut prob, t_blk, pb);
                        add_prob(&mut prob, e_blk, pb);
                    } else if at_bool(&ends_in_cold_trap, t_blk) && !at_bool(&ends_in_cold_trap, e_blk) {
                        add_prob(&mut prob, t_blk, 1);
                        add_prob(&mut prob, e_blk, pb);
                    } else if at_bool(&ends_in_cold_trap, e_blk) && !at_bool(&ends_in_cold_trap, t_blk) {
                        add_prob(&mut prob, e_blk, 1);
                        add_prob(&mut prob, t_blk, pb);
                    } else if at_u32(&loop_step, t_blk) > 1 && at_bool(&is_tail_epilogue, e_blk) {
                        // Fast-path vector/unrolled guard: fast loop is the common case.
                        add_prob(&mut prob, t_blk, pb);
                        add_prob(&mut prob, e_blk, (pb / 8).max(1));
                    } else if at_u32(&loop_step, e_blk) > 1 && at_bool(&is_tail_epilogue, t_blk) {
                        add_prob(&mut prob, e_blk, pb);
                        add_prob(&mut prob, t_blk, (pb / 8).max(1));
                    } else {
                        let half = (pb / 2).max(1);
                        add_prob(&mut prob, t_blk, half);
                        add_prob(&mut prob, e_blk, half);
                    }
                }
            }
            _ => {
                let mut fwd_normal = 0u32;
                each(&view, term, |item| {
                    if let Item::Edge(edge) = item {
                        let tb = to_idx(edge.block);
                        if tb > b && tb < n_tracked && !at_bool(&ends_in_cold_trap, tb) {
                            fwd_normal += 1;
                        }
                    }
                });
                let share = (pb / fwd_normal.max(1)).max(1);
                each(&view, term, |item| {
                    if let Item::Edge(edge) = item {
                        let tb = to_idx(edge.block);
                        if tb > b && tb < n_tracked {
                            let add = if at_bool(&ends_in_cold_trap, tb) { 1 } else { share };
                            add_prob(&mut prob, tb, add);
                        }
                    }
                });
            }
        }
    }

    // 5. Sum weighted microarchitectural port costs across every block.
    let mut total_cost = 0u64;
    let mut loop_body_cost = 0u64;

    for b in 0..blocks {
        let range = view.block(Block(to_u32(b)));
        let (d, step_k, is_tail, pb, is_hdr) = if b < n_tracked {
            (
                at_u8(&depth, b),
                at_u32(&loop_step, b).max(1),
                at_bool(&is_tail_epilogue, b),
                at_u32(&prob, b).max(1),
                at_bool(&has_backedge, b),
            )
        } else {
            (0u8, 1u32, false, BASE_PROB, false)
        };

        let mut raw_cost = 0u64;
        for inst in range.start..range.end {
            raw_cost = raw_cost.saturating_add(inst_port_cost(
                &view,
                inst,
                b,
                range.start,
                is_hdr,
                model,
            ));
        }
        if raw_cost == 0 {
            continue;
        }

        // Multi-accumulator ILP credit for 4x+ unrolled loops (breaks serial loop-carried chains).
        let effective_raw = if step_k >= 4 && d >= 1 {
            (raw_cost * 80) / 100
        } else {
            raw_cost
        };

        // Loop-depth execution weight: 1x outside loops, 16x depth-1, 256x depth-2 (`16^d`).
        let base_depth_weight: u64 = match d {
            0 => 1,
            1 => 16,
            2 => 256,
            _ => 1u64 << (4 * u32::from(d.min(5))),
        };
        let depth_weight = if is_tail && d >= 1 {
            // Scalar tail epilogue runs `< K` remainder iterations rather than the full trip count.
            (base_depth_weight / 8).max(2)
        } else {
            base_depth_weight
        };

        let denom = u64::from(BASE_PROB) * u64::from(step_k);
        let weighted = effective_raw
            .saturating_mul(depth_weight)
            .saturating_mul(u64::from(pb))
            / denom;
        let weighted = weighted.max(1);

        total_cost = total_cost.saturating_add(weighted);
        if d >= 1 {
            loop_body_cost = loop_body_cost.saturating_add(weighted);
        }
    }

    RegionCost {
        total_cost,
        loop_body_cost,
        inst_count: to_u32(view.len()),
    }
}

/// Runs the e-graph algebraic + strength-reduction + LICM + propagate + DCE + CFG-coalesce bundle.
fn run_egraph_bundle<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    for _ in 0..2 {
        let step = aegraph_optimize(cur.view(), alloc)?;
        let (sr_func, sr_count) = materialize_strength_reduction(step.func.view(), alloc)?;
        let (licm_func, hoisted) = hoist_loop_invariants(sr_func.view(), alloc)?;
        let prop = propagate(licm_func.view(), alloc)?;
        let cleaned = dead(prop.view(), alloc)?;
        let coalesced = coalesce_cfg(cleaned.view(), alloc)?;

        let changed = coalesced.ops.len() != cur.ops.len()
            || step.stats.gvn_hits > 0
            || step.stats.identities_folded > 0
            || step.stats.constants_folded > 0
            || sr_count > 0
            || hoisted > 0;
        cur = coalesced;
        if !changed {
            break;
        }
    }
    Ok(cur)
}

#[inline]
fn supported_vec_width(model: PortCostModel) -> u32 {
    match model.vec_width_bytes {
        64 => 64,
        32 => 32,
        _ => 16,
    }
}

/// Strategy A: `egraph -> bce -> ifconv -> loopnest -> vectorize -> egraph`.
/// Algebraic-first + bounds-check elimination + branchless masked SIMD / loop-nest fusion.
fn run_strategy_a<A: Allocator + Copy>(
    view: View<'_>,
    model: PortCostModel,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let f1 = run_egraph_bundle(view, alloc)?;
    let f2 = eliminate_redundant_checks(f1.view(), alloc)?.func;
    let f3 = if_convert(f2.view(), 8, alloc)?.func;
    let f4 = fuse_adjacent_loops(f3.view(), alloc)?.func;
    let f5 = interchange_and_tile_loops(f4.view(), 0, alloc)?.func;
    let f6 = auto_vectorize_loops_with_width(f5.view(), supported_vec_width(model), alloc)?.func;
    run_egraph_bundle(f6.view(), alloc)
}

/// Strategy B: `loopnest -> egraph -> bce -> ifconv -> slp -> unroll -> egraph`.
/// Loop-fusion/interchange-first unlocking cross-loop algebraic cancellation, SLP, and
/// 4x multi-accumulator scalar unrolling.
fn run_strategy_b<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let f1 = fuse_adjacent_loops(view, alloc)?.func;
    let f2 = interchange_and_tile_loops(f1.view(), 0, alloc)?.func;
    let f3 = run_egraph_bundle(f2.view(), alloc)?;
    let f4 = eliminate_redundant_checks(f3.view(), alloc)?.func;
    let f5 = if_convert(f4.view(), 8, alloc)?.func;
    let f6 = slp_vectorize(f5.view(), alloc)?.func;
    let f7 = unroll_scalar_loops(f6.view(), alloc)?.func;
    run_egraph_bundle(f7.view(), alloc)
}

/// Strategy C: `egraph -> bce -> unswitch -> egraph -> vectorize/memchr`.
/// Unswitching-first unlocking invariant branch hoisting + SIMD / byte-scan vectorization.
fn run_strategy_c<A: Allocator + Copy>(
    view: View<'_>,
    model: PortCostModel,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let f1 = run_egraph_bundle(view, alloc)?;
    let f2 = eliminate_redundant_checks(f1.view(), alloc)?.func;
    let f3 = unswitch_loops(f2.view(), 4, alloc)?.func;
    let f4 = run_egraph_bundle(f3.view(), alloc)?;
    let f5 = auto_vectorize_loops_with_width(f4.view(), supported_vec_width(model), alloc)?.func;
    let f6 = vectorize_memchr_loops(f5.view(), alloc)?.func;
    run_egraph_bundle(f6.view(), alloc)
}

/// Explores competing multi-pass RVSDG / e-graph co-optimization trajectories over `view` and
/// deterministically selects the verified candidate with the strictly lowest estimated
/// execution-port cost under `model`.
pub fn co_optimize_regions<A: Allocator + Copy>(
    view: View<'_>,
    model: PortCostModel,
    alloc: A,
) -> Result<RvsdgOutput<A>, SimplifyError> {
    verify(view).map_err(SimplifyError::Verify)?;

    let initial_cost = estimate_function_cost(view, model).total_cost;
    let mut best_func = view.to_func_in(alloc);
    let mut best_cost = initial_cost;
    let mut chosen_strategy: &'static str = "baseline";
    let mut rounds = 0u32;

    // Strategy A: algebraic-first + masked SIMD / loop-nest fusion.
    let cand_a = run_strategy_a(view, model, alloc)?;
    verify(cand_a.view()).map_err(SimplifyError::Verify)?;
    rounds += 1;
    let cost_a = estimate_function_cost(cand_a.view(), model).total_cost;
    if cost_a < best_cost {
        best_cost = cost_a;
        best_func = cand_a;
        chosen_strategy = "strategy_a_egraph_simd";
    }

    // Strategy B: loop-fusion/interchange-first unlocking cross-loop cancellation & scalar unroll.
    let cand_b = run_strategy_b(view, alloc)?;
    verify(cand_b.view()).map_err(SimplifyError::Verify)?;
    rounds += 1;
    let cost_b = estimate_function_cost(cand_b.view(), model).total_cost;
    if cost_b < best_cost {
        best_cost = cost_b;
        best_func = cand_b;
        chosen_strategy = "strategy_b_loopnest_unroll";
    }

    // Strategy C: unswitching-first unlocking invariant branch hoisting + SIMD / memchr.
    let cand_c = run_strategy_c(view, model, alloc)?;
    verify(cand_c.view()).map_err(SimplifyError::Verify)?;
    rounds += 1;
    let cost_c = estimate_function_cost(cand_c.view(), model).total_cost;
    if cost_c < best_cost {
        best_cost = cost_c;
        best_func = cand_c;
        chosen_strategy = "strategy_c_unswitch_simd";
    }

    verify(best_func.view()).map_err(SimplifyError::Verify)?;

    Ok(RvsdgOutput {
        func: best_func,
        stats: RvsdgStats {
            rounds,
            initial_cost,
            final_cost: best_cost,
            chosen_strategy,
        },
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
        func::Conv,
        simplify::validate::{ValidationOpts, validate_equivalence},
    };

    /// Filtered reduction loop:
    /// `fn filtered_sum(n: u64, thresh: u64) -> u64 { for i in 0..n { if i > thresh { acc += i; } } }`
    fn build_filtered_reduction_loop() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, body, then_b, join, exit] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let thresh = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[z, z]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let gt = b.icmp(IntCond::Ugt, i, thresh);
        b.br(Flags::NONE, gt, (then_b, &[]), (join, &[acc]));

        b.begin(then_b);
        let acc_t = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((join, &[acc_t]));

        b.begin(join);
        let acc_next = b.param(Ty::I64);
        let c1 = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((hdr, &[i2, acc_next]));

        b.begin(exit);
        b.ret(&[acc]);
        b.finish_checked().unwrap()
    }

    /// Division/modulo + invariant-expression kernel:
    /// `fn div_kernel(n: u64, k: u64) -> u64 { for i in 0..n { acc += (i / 10) + (i % 10) + (k * 4 - k * 4); } }`
    fn build_div_kernel() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let k = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[z, z]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let c10 = b.iconst(Ty::I64, 10);
        let c4 = b.iconst(Ty::I64, 4);
        let c1 = b.iconst(Ty::I64, 1);
        let q = b.binary(Op::Udiv, Flags::NONE, i, c10);
        let r = b.binary(Op::Urem, Flags::NONE, i, c10);
        let k4a = b.binary(Op::Mul, Flags::NONE, k, c4);
        let k4b = b.binary(Op::Mul, Flags::NONE, k, c4);
        let cancel = b.binary(Op::Sub, Flags::NONE, k4a, k4b);
        let qr = b.binary(Op::Add, Flags::NONE, q, r);
        let term = b.binary(Op::Add, Flags::NONE, qr, cancel);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, term);
        let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((hdr, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn rvsdg_co_optimizer_beats_baseline_cost_and_proves_equivalence() {
        let model = PortCostModel::apple_silicon();
        let val_opts = ValidationOpts {
            max_steps: 25_000,
            random_samples: 64,
            ..ValidationOpts::default_fast()
        };

        // 1. Filtered reduction loop: eliminates interior branch & unlocks 4x multi-acc unrolling!
        let filtered = build_filtered_reduction_loop();
        let opt_filtered = co_optimize_regions(filtered.view(), model, TestHeap).unwrap();
        assert_eq!(opt_filtered.stats.rounds, 3);
        assert!(
            opt_filtered.stats.final_cost < opt_filtered.stats.initial_cost,
            "co-optimizer must strictly beat baseline cost on filtered reduction ({} >= {})",
            opt_filtered.stats.final_cost,
            opt_filtered.stats.initial_cost
        );
        assert_ne!(opt_filtered.stats.chosen_strategy, "baseline");

        let report_f = validate_equivalence(
            filtered.view(),
            opt_filtered.func.view(),
            val_opts,
            TestHeap,
        );
        assert!(
            report_f.is_valid(),
            "filtered reduction co-optimization diverged: {:?}",
            report_f.counterexample
        );

        // Also check concrete trip counts directly with `eval::run_in`.
        for &(nv, tv) in &[(0u128, 0u128), (1, 0), (4, 2), (17, 7), (35, 10), (40, 100)] {
            let Outcome::Returned(a) =
                eval::run_in(filtered.view(), &[Val::Int(nv), Val::Int(tv)], 20_000, TestHeap).unwrap()
            else {
                panic!("orig filtered eval failed");
            };
            let Outcome::Returned(b) = eval::run_in(
                opt_filtered.func.view(),
                &[Val::Int(nv), Val::Int(tv)],
                20_000,
                TestHeap,
            )
            .unwrap() else {
                panic!("opt filtered eval failed");
            };
            assert_eq!(a, b, "filtered mismatch at n={nv}, thresh={tv}");
        }

        // 2. Division/modulo + redundant algebra kernel: strength-reduces udiv/urem + cancels k*4-k*4 + unrolls!
        let div_fn = build_div_kernel();
        let opt_div = co_optimize_regions(div_fn.view(), model, TestHeap).unwrap();
        assert_eq!(opt_div.stats.rounds, 3);
        assert!(
            opt_div.stats.final_cost < opt_div.stats.initial_cost,
            "co-optimizer must strictly beat baseline cost on div kernel ({} >= {})",
            opt_div.stats.final_cost,
            opt_div.stats.initial_cost
        );
        assert_ne!(opt_div.stats.chosen_strategy, "baseline");

        let report_d = validate_equivalence(
            div_fn.view(),
            opt_div.func.view(),
            val_opts,
            TestHeap,
        );
        assert!(
            report_d.is_valid(),
            "div kernel co-optimization diverged: {:?}",
            report_d.counterexample
        );

        for &(nv, kv) in &[(0u128, 5u128), (1, 9), (9, 3), (10, 7), (25, 11), (53, 42)] {
            let Outcome::Returned(a) =
                eval::run_in(div_fn.view(), &[Val::Int(nv), Val::Int(kv)], 50_000, TestHeap).unwrap()
            else {
                panic!("orig div eval failed");
            };
            let Outcome::Returned(b) = eval::run_in(
                opt_div.func.view(),
                &[Val::Int(nv), Val::Int(kv)],
                50_000,
                TestHeap,
            )
            .unwrap() else {
                panic!("opt div eval failed");
            };
            assert_eq!(a, b, "div mismatch at n={nv}, k={kv}");
        }
    }

    #[test]
    fn port_cost_model_distinguishes_apple_silicon_sha3_and_zen5_widths() {
        let m_apple = PortCostModel::apple_silicon();
        let m_zen5 = PortCostModel::x86_64_zen5();
        assert!(m_apple.has_sha3 && m_apple.has_fma);
        assert!(!m_zen5.has_sha3 && m_zen5.has_fma);
        assert_eq!(m_apple.vec_width_bytes, 16);
        assert_eq!(m_zen5.vec_width_bytes, 32);

        // Build `eor3`-fusible `((a ^ b) ^ c)` function: Apple Silicon (`has_sha3 = true`)
        // charges strictly less than Zen 5 (`has_sha3 = false`, 6-wide ALU).
        let mut b = Builder::<TestHeap, Full>::new_in(32, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let z = b.param(Ty::I64);
        let xy = b.binary(Op::Xor, Flags::NONE, x, y);
        let xyz = b.binary(Op::Xor, Flags::NONE, xy, z);
        b.ret(&[xyz]);
        let func = b.finish_checked().unwrap();

        let c_apple = estimate_function_cost(func.view(), m_apple);
        let c_zen5 = estimate_function_cost(func.view(), m_zen5);
        assert_eq!(c_apple.inst_count, func.view().len() as u32);
        assert_eq!(c_apple.loop_body_cost, 0);
        assert!(
            c_apple.total_cost < c_zen5.total_cost,
            "Apple Silicon SHA3 eor3 + 8-wide ALU must cost less than Zen 5 ({} vs {})",
            c_apple.total_cost,
            c_zen5.total_cost
        );
    }
}

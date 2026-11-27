//! Intra-Block Register-Pressure-Aware Critical-Path DAG List Scheduler (`blaze_lir::sched`).
//!
//! Reorders pure, side-effect-free instructions within each basic block (strictly between
//! `Op::Param`s and side-effecting barriers such as `Store`/`Call`/`Atomic*`/`Trap`/terminator)
//! using Sethi-Ullman / ILP-with-RegPressure list scheduling:
//!
//! - Under low register pressure (`region_live < seg_pressure_high`, where `seg_pressure_high`
//!   subtracts unread live-through GPRs from `TOTAL_GPR_BUDGET`, capped at `REG_PRESSURE_HIGH`):
//!   prioritizes ready-now ops and highest bottom-up critical-path latency `height` — pulling
//!   long-latency `Load` (4 cyc) and `Umulhi`/`Mul`/`Fmul`/`Fadd`/`Fma` (3 cyc) ops forward and
//!   interleaving independent chains — breaking ties by smallest `live_delta`.
//! - Under high register pressure (`region_live >= seg_pressure_high`): prioritizes strictly
//!   draining ops (`live_delta < 0`) first, then ready non-exit `Op::Load` ops and their
//!   address-forming precursors while total live GPRs stay below `LOAD_HOIST_PRESSURE_LIMIT`,
//!   then remaining `live_delta >= 0` ops by smallest `live_delta`, avoiding unnecessary register
//!   spills, breaking ties by highest `height` then original index.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    op::{Field, Layout},
    verify,
};

/// Telemetry from [`schedule_dag`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SchedStats {
    pub blocks_scheduled: u32,
    pub insts_reordered: u32,
}

/// Output of [`schedule_dag`].
#[derive(Debug)]
pub struct SchedOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SchedStats,
}

#[inline]
fn is_schedulable_pure(op: Op, flags: Flags) -> bool {
    if op.info().flags.meanings_only(flags).0 != 0 {
        return false;
    }
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Splat
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Umulhi
            | Op::Smulhi
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Icmp
            | Op::Fcmp
            | Op::Select
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Bitcast
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fma
            | Op::Fmuladd
            | Op::Fneg
            | Op::Fabs
            | Op::PtrAdd
            | Op::Load
    )
}

#[inline]
fn op_latency(op: Op) -> u32 {
    match op {
        Op::Load => 4,
        Op::Mul | Op::Umulhi | Op::Smulhi | Op::Fmul | Op::Fadd | Op::Fsub | Op::Fma | Op::Fmuladd => 3,
        Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide | Op::Icmp | Op::Fcmp => 0,
        _ => 1,
    }
}

/// High-pressure threshold for intra-segment live values.
/// Leaves headroom for outer-loop live-through values so total live GPRs stay <= 25 on AArch64.
const REG_PRESSURE_HIGH: usize = 16;

/// Target total live scalar values (live-through + intra-segment) before switching
/// from ILP-first to pressure-draining priority. Leaves 6 GPRs of headroom out of
/// AArch64's 28 allocatable leaf GPRs (`x0..x15, x19..x30`) for simultaneous live-range
/// overlaps across `csel` chains while keeping full ILP overlap on hot loops.
const TOTAL_GPR_BUDGET: usize = 22;

/// Total live GPR ceiling (`region_live + seg_live_through`) below which ready non-exit
/// `Op::Load` instructions and their address-forming precursors still launch ahead of
/// `delta >= 0` ALU ops under `high_pressure`. Keeps 1 GPR of headroom below AArch64's
/// 28-GPR leaf pool (`x0..x15, x19..x30`, with `x29` enabled as the 28th allocatable GPR
/// in zero-call fixed-frame leaf functions) so dual L1/L2 hash-table probes (e.g.
/// `long_table` and `short_table`) can both hoist at `total_live == 26` instead of
/// stalling behind 1-cycle bitwise ALU ops.
const LOAD_HOIST_PRESSURE_LIMIT: usize = 27;

/// Total live GPR pressure (`region_live + seg_live_through`) at or above which pressure-increasing
/// (`live_delta > 0`) non-`Load` `only_feeds_exit` ALU ops defer behind ready non-exit work so they
/// do not allocate long-lived exit-only registers across the active block body, while draining
/// (`live_delta <= 0`) exit-only recurrences schedule immediately once their operands have no
/// remaining non-exit readers.
const EXIT_ALU_DEFER_PRESSURE: usize = 14;

/// Bounds on whole-function backward CFG liveness (Rule 18). Above these limits,
/// `seg_live_through` falls back to `0` (`seg_pressure_high == REG_PRESSURE_HIGH`).
const MAX_LIVENESS_BLOCKS: usize = 512;
const MAX_LIVENESS_INSTS: usize = 8192;
const MAX_LIVENESS_WORDS: usize = 32_768;
const MAX_LIVENESS_ITERS: usize = 16;

/// Sentinel in `use_block` indicating an SSA value is used in more than one basic block.
const MULTI_BLOCK: u32 = u32::MAX - 1;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at_u64(s: &[u64], i: usize) -> u64 { s.get(i).copied().unwrap_or(0) }
#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 { s.get(i).copied().unwrap_or(0) }
#[inline]
fn at_u16(s: &[u16], i: usize) -> u16 { s.get(i).copied().unwrap_or(0) }

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_usize(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_head(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(usize::MAX)
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
fn bit_set(bits: &mut [u64], base: usize, vi: usize) {
    let word = vi >> 6;
    let bit = vi & 63;
    if let Some(slot) = bits.get_mut(base + word) {
        *slot |= 1u64 << bit;
    }
}

#[inline]
fn bit_get(bits: &[u64], base: usize, vi: usize) -> bool {
    let word = vi >> 6;
    let bit = vi & 63;
    (at_u64(bits, base + word) & (1u64 << bit)) != 0
}

#[inline]
fn counts_for_pressure(ty: Ty) -> bool {
    !ty.is_none()
}

#[inline]
fn is_zero_pressure_op(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::SlotAddr
            | Op::FuncAddr
            | Op::DataAddr
            | Op::Null
            | Op::Uninit
            | Op::Icmp
            | Op::Fcmp
    )
}

#[inline]
fn counts_for_live_pressure(ty: Ty, op: Op) -> bool {
    counts_for_pressure(ty) && !is_zero_pressure_op(op)
}

#[inline]
fn is_gpr_live_pressure(view: &View<'_>, vi: usize) -> bool {
    let ty = Ty(at_u16(view.tys, vi));
    ty.is_scalar() && (ty.is_int() || ty.is_ptr()) && !is_zero_pressure_op(view.op(vi))
}

/// Per-block backward CFG liveness over non-constant scalar GPR values (`live_out[b]`).
struct CfgLiveness<A: Allocator> {
    words: usize,
    live_out: Vec<u64, A>,
}

fn compute_cfg_liveness<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: A,
) -> Option<CfgLiveness<A>> {
    let blocks = view.blocks();
    let len = view.len();
    let words = len.div_ceil(64);
    if blocks == 0
        || words == 0
        || blocks > MAX_LIVENESS_BLOCKS
        || len > MAX_LIVENESS_INSTS
        || blocks.saturating_mul(words) > MAX_LIVENESS_WORDS
    {
        return None;
    }

    let total_words = blocks * words;
    let mut upward_use = filled(total_words, 0u64, &alloc);
    let mut live_in = filled(total_words, 0u64, &alloc);
    let mut live_out = filled(total_words, 0u64, &alloc);
    let mut succ_starts: Vec<usize, A> = Vec::with_capacity_in(blocks + 1, alloc);
    let mut succ_blocks: Vec<usize, A> = Vec::with_capacity_in(blocks * 2, alloc);

    for b in 0..blocks {
        succ_starts.push(succ_blocks.len());
        let range = view.block(Block(to_u32(b)));
        let base = b * words;
        for inst in range.start..range.end {
            each(view, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if vi < len
                        && (vi < range.start || vi >= range.end)
                        && is_gpr_live_pressure(view, vi)
                    {
                        bit_set(&mut upward_use, base, vi);
                    }
                }
                Item::Edge(edge) => {
                    let succ = to_idx(edge.block);
                    if succ < blocks {
                        succ_blocks.push(succ);
                    }
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = to_idx(at_u32(view.extra, start + k));
                            if vi < len
                                && (vi < range.start || vi >= range.end)
                                && is_gpr_live_pressure(view, vi)
                            {
                                bit_set(&mut upward_use, base, vi);
                            }
                        }
                    }
                }
            });
        }
    }
    succ_starts.push(succ_blocks.len());

    for i in 0..total_words {
        set_at(&mut live_in, i, at_u64(&upward_use, i));
    }

    for _ in 0..MAX_LIVENESS_ITERS {
        let mut changed = false;
        for b in (0..blocks).rev() {
            let range = view.block(Block(to_u32(b)));
            let b_base = b * words;
            let s_start = at_usize(&succ_starts, b);
            let s_end = at_usize(&succ_starts, b + 1);
            for idx in s_start..s_end {
                let s = at_usize(&succ_blocks, idx);
                let s_base = s * words;
                for w in 0..words {
                    let incoming = at_u64(&live_in, s_base + w);
                    if incoming != 0 {
                        let prev = at_u64(&live_out, b_base + w);
                        let next = prev | incoming;
                        if next != prev {
                            set_at(&mut live_out, b_base + w, next);
                        }
                    }
                }
            }
            // `live_in[b] = upward_use[b] | (live_out[b] \ [range.start, range.end))`
            let first_def_w = range.start >> 6;
            let last_def_w = if range.end > range.start {
                (range.end - 1) >> 6
            } else {
                usize::MAX
            };
            let def_low_mask = !0u64 << (range.start & 63);
            let def_end_rem = range.end & 63;
            let def_high_mask = if def_end_rem == 0 {
                !0u64
            } else {
                (1u64 << def_end_rem) - 1
            };
            for w in 0..words {
                let mut out_w = at_u64(&live_out, b_base + w);
                if range.end > range.start && w >= first_def_w && w <= last_def_w {
                    let def_mask = if first_def_w == last_def_w {
                        def_low_mask & def_high_mask
                    } else if w == first_def_w {
                        def_low_mask
                    } else if w == last_def_w {
                        def_high_mask
                    } else {
                        !0u64
                    };
                    out_w &= !def_mask;
                }
                let prev = at_u64(&live_in, b_base + w);
                let next = at_u64(&upward_use, b_base + w) | out_w;
                if next != prev {
                    set_at(&mut live_in, b_base + w, next);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    Some(CfgLiveness { words, live_out })
}

#[inline]
fn record_use(v: u32, b32: u32, inst: usize, use_block: &mut [u32], last_use_inst: &mut [u32]) {
    let vi = to_idx(v);
    let prev = at_u32(use_block, vi);
    if prev == NONE {
        set_at(use_block, vi, b32);
    } else if prev != b32 {
        set_at(use_block, vi, MULTI_BLOCK);
    }
    set_at(last_use_inst, vi, to_u32(inst));
}

/// Returns `true` when `inst` is a scalar integer `Op::Shl(_, k)` with an `Op::Iconst` shift
/// amount `k` in `{1, 2, 3}` (matching AArch64's `[base, idx, lsl #1/2/3]` addressing mode).
#[inline]
fn is_folded_shl_shift(view: &View<'_>, inst: usize) -> bool {
    if inst >= view.len() || view.op(inst) != Op::Shl {
        return false;
    }
    let ty = Ty(at_u16(view.tys, inst));
    if !ty.is_scalar() || !ty.is_int() {
        return false;
    }
    let at = to_idx(at_u32(view.data, inst));
    let rhs = to_idx(at_u32(view.extra, at + 1));
    rhs < view.len() && view.op(rhs) == Op::Iconst && matches!(at_u32(view.data, rhs), 1..=3)
}

/// Returns `true` when `off` is an offset operand that AArch64 folds directly into a
/// `Load`/`Store` addressing mode: an `Op::Iconst`, a scaled `Op::Shl(idx, 1..=3)`, or a
/// `Zext`/`Sext` of either.
#[inline]
fn is_folded_ptr_add_offset(view: &View<'_>, off: usize) -> bool {
    if off >= view.len() {
        return false;
    }
    let inner = match view.op(off) {
        Op::Zext | Op::Sext => to_idx(at_u32(view.data, off)),
        _ => off,
    };
    if inner >= view.len() {
        return false;
    }
    view.op(inner) == Op::Iconst || is_folded_shl_shift(view, inner)
}

#[inline]
fn is_folded_addr_inst(
    vi: usize,
    seg_start: usize,
    seg_end: usize,
    is_folded_addr: &[bool],
) -> bool {
    vi >= seg_start && vi < seg_end && at_bool(is_folded_addr, vi - seg_start)
}

#[inline]
fn calc_live_delta(
    view: &View<'_>,
    cand: usize,
    seg_start: usize,
    seg_end: usize,
    inst_defines_live: &[bool],
    is_folded_addr: &[bool],
    op_starts: &[usize],
    inst_ops: &[usize],
    seg_rem_uses: &[u32],
    seg_live_out: &[bool],
) -> i32 {
    let mut delta: i32 = if at_bool(inst_defines_live, cand) && !at_bool(is_folded_addr, cand) {
        1
    } else {
        0
    };
    let start = at_usize(op_starts, cand);
    let end = at_usize(op_starts, cand + 1);
    for idx in start..end {
        let vi = at_usize(inst_ops, idx);
        if at_u32(seg_rem_uses, vi) == 1
            && !at_bool(seg_live_out, vi)
            && !is_folded_addr_inst(vi, seg_start, seg_end, is_folded_addr)
            && counts_for_live_pressure(Ty(at_u16(view.tys, vi)), view.op(vi))
        {
            delta -= 1;
        }
    }
    delta
}

/// Returns `true` when `inst` is an `Op::Icmp` testing `Eq` or `Ne` against integer constant `0`.
#[inline]
fn is_zero_eq_icmp(view: &View<'_>, inst: usize) -> bool {
    if inst >= view.len() || view.op(inst) != Op::Icmp {
        return false;
    }
    let cat = to_idx(at_u32(view.data, inst));
    let Some(pred) = IntCond::from_word(at_u32(view.extra, cat)) else {
        return false;
    };
    if !matches!(pred, IntCond::Eq | IntCond::Ne) {
        return false;
    }
    let lhs = to_idx(at_u32(view.extra, cat + 1));
    let rhs = to_idx(at_u32(view.extra, cat + 2));
    let lhs_zero = lhs < view.len() && view.op(lhs) == Op::Iconst && at_u32(view.data, lhs) == 0;
    let rhs_zero = rhs < view.len() && view.op(rhs) == Op::Iconst && at_u32(view.data, rhs) == 0;
    lhs_zero || rhs_zero
}

/// Returns `(canon_lhs, canon_rhs, is_zero_eq)` when `inst` is an `Op::Select` whose condition
/// is a same-function `Op::Icmp` comparing `(lhs, rhs)`.
///
/// `is_zero_eq` is `true` when the comparison is `Eq` or `Ne` against integer constant `0`
/// (which can reuse an upstream `subs`/`adds` Z flag with zero `cmp` instruction and keeps
/// non-zero `(x, k)` select families contiguous).
#[inline]
fn select_cmp_info(view: &View<'_>, inst: usize) -> Option<(u32, u32, bool)> {
    if view.op(inst) != Op::Select {
        return None;
    }
    let at = to_idx(at_u32(view.data, inst));
    let cond_vi = to_idx(at_u32(view.extra, at));
    if cond_vi >= view.len() || view.op(cond_vi) != Op::Icmp {
        return None;
    }
    let cat = to_idx(at_u32(view.data, cond_vi));
    let lhs = at_u32(view.extra, cat + 1);
    let rhs = at_u32(view.extra, cat + 2);
    let is_zero_eq = is_zero_eq_icmp(view, cond_vi);
    let (a, b) = if lhs <= rhs { (lhs, rhs) } else { (rhs, lhs) };
    Some((a, b, is_zero_eq))
}

/// 5-tier total rank under high register pressure:
/// - `0`: strictly draining (`delta < 0`)
/// - `1`: urgent non-exit `Op::Load` (`load_urgency == 2`)
/// - `2`: urgent non-exit load-address precursor (`load_urgency == 1`)
/// - `3`: non-exit `Op::Select` on `x == 0` / `x != 0` (reuses `subs`/`adds` Z flag)
/// - `4`: all other `delta >= 0` ops
#[inline]
fn high_pressure_rank(delta: i32, load_urgency: u8, zero_eq_select: bool) -> u8 {
    if delta < 0 {
        0
    } else if load_urgency >= 2 {
        1
    } else if load_urgency == 1 {
        2
    } else if zero_eq_select {
        3
    } else {
        4
    }
}

/// Maximum zero-cost conversion hops (`Zext`/`Sext`/`Trunc`/`Bitcast`) followed backwards when
/// tracing a back-edge argument to its defining root instruction.
const MAX_CONVERT_CHAIN: usize = 16;

/// Maximum intermediate merge/latch `Op::Param` hops followed backwards from a loop back-edge.
const MAX_PARAM_HOPS: usize = 4;

/// Maximum transitive `(merge_param, loop_param)` pairs traced per function.
const MAX_PARAM_PAIRS: usize = 256;

#[inline]
fn block_of_inst(view: &View<'_>, inst: usize) -> usize {
    view.block_starts
        .partition_point(|&start| to_idx(start) <= inst)
        .saturating_sub(1)
}

/// Whole-function loop-carried block-parameter replacement edges computed once per function.
struct GlobalLoopReps<A: Allocator> {
    is_loop_replaced_param: Vec<bool, A>,
    head: Vec<usize, A>,
    param: Vec<usize, A>,
    next: Vec<usize, A>,
}

struct KillCtx<'a> {
    seg_start: usize,
    only_feeds_exit: &'a [bool],
    is_iv_term_update: &'a [bool],
    is_exit_leaf: &'a [bool],
    is_load: &'a [bool],
    op_starts: &'a [usize],
    inst_ops: &'a [usize],
    rem_non_exit_uses: &'a [u16],
    seg_rem_uses: &'a [u32],
    seg_stamp: &'a [u32],
    seg_live_out: &'a [bool],
    rep_param_stamp: &'a [u32],
    is_loop_replaced_param: &'a [bool],
    cur_stamp: u32,
    rep_head: &'a [usize],
    rep_param: &'a [usize],
    rep_next: &'a [usize],
    global_rep_head: &'a [usize],
    global_rep_param: &'a [usize],
    global_rep_next: &'a [usize],
}

impl KillCtx<'_> {
    #[inline]
    fn replaces_live_param(
        &self,
        start: usize,
        end: usize,
        mut cur: usize,
        rep_param: &[usize],
        rep_next: &[usize],
    ) -> bool {
        while cur != usize::MAX {
            let p = at_usize(rep_param, cur);
            if at_u32(self.seg_stamp, p) == self.cur_stamp {
                let mut self_reads = 0u32;
                for idx in start..end {
                    if at_usize(self.inst_ops, idx) == p {
                        self_reads = 1;
                        break;
                    }
                }
                if at_u32(self.seg_rem_uses, p) > self_reads {
                    return true;
                }
            }
            cur = at_head(rep_next, cur);
        }
        false
    }

    #[inline]
    fn blocks_operand_kill(&self, cand: usize) -> bool {
        let start = at_usize(self.op_starts, cand);
        let end = at_usize(self.op_starts, cand + 1);
        if self.replaces_live_param(
            start,
            end,
            at_head(self.rep_head, cand),
            self.rep_param,
            self.rep_next,
        ) {
            return true;
        }
        if at_bool(self.is_iv_term_update, cand) {
            return self.replaces_live_param(
                start,
                end,
                at_head(self.global_rep_head, self.seg_start + cand),
                self.global_rep_param,
                self.global_rep_next,
            );
        }
        if !at_bool(self.only_feeds_exit, cand) {
            return false;
        }
        let leaf = at_bool(self.is_exit_leaf, cand);
        // Defer non-`Load` exit-only intermediates (e.g. bitfield extractions from a loaded
        // packed word) as well as exit leaves until non-exit readers of a non-live-out operand
        // finish, so the operand and its extracted children do not stay live together.
        // Non-leaf `Op::Load` ops keep full ILP unless reading/replacing a live loop param.
        let defer_non_liveout = leaf || !at_bool(self.is_load, cand);
        for idx in start..end {
            let vi = at_usize(self.inst_ops, idx);
            if at_u16(self.rem_non_exit_uses, vi) > 0
                && (at_bool(self.is_loop_replaced_param, vi)
                    || at_u32(self.rep_param_stamp, vi) == self.cur_stamp
                    || (defer_non_liveout && !at_bool(self.seg_live_out, vi)))
            {
                return true;
            }
        }
        self.replaces_live_param(
            start,
            end,
            at_head(self.global_rep_head, self.seg_start + cand),
            self.global_rep_param,
            self.global_rep_next,
        )
    }
}

#[inline]
fn record_replaced_param<A: Allocator>(
    node: usize,
    param_vi: usize,
    rep_head: &mut [usize],
    rep_param: &mut Vec<usize, A>,
    rep_next: &mut Vec<usize, A>,
) {
    let mut cur = at_head(rep_head, node);
    while cur != usize::MAX {
        if at_usize(rep_param, cur) == param_vi {
            return;
        }
        cur = at_head(rep_next, cur);
    }
    let entry = rep_param.len();
    rep_param.push(param_vi);
    rep_next.push(at_head(rep_head, node));
    set_at(rep_head, node, entry);
}

fn record_rep_with_converts<A: Allocator>(
    view: &View<'_>,
    arg_vi: usize,
    param_vi: usize,
    head: &mut [usize],
    param: &mut Vec<usize, A>,
    next: &mut Vec<usize, A>,
) -> usize {
    let len = view.len();
    let mut cur_vi = arg_vi;
    record_replaced_param(cur_vi, param_vi, head, param, next);
    for _ in 0..MAX_CONVERT_CHAIN {
        if !matches!(
            view.op(cur_vi),
            Op::Zext | Op::Sext | Op::Trunc | Op::Bitcast
        ) {
            break;
        }
        let mut src_vi = usize::MAX;
        each(view, cur_vi, |sub| {
            if let Item::Value(v) = sub {
                src_vi = to_idx(v);
            }
        });
        if src_vi >= len || src_vi == cur_vi || src_vi == param_vi {
            break;
        }
        cur_vi = src_vi;
        record_replaced_param(cur_vi, param_vi, head, param, next);
    }
    cur_vi
}

fn collect_global_loop_reps<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: A,
) -> GlobalLoopReps<A> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 {
        return GlobalLoopReps {
            is_loop_replaced_param: Vec::new_in(alloc),
            head: Vec::new_in(alloc),
            param: Vec::new_in(alloc),
            next: Vec::new_in(alloc),
        };
    }

    // Iterative DFS from entry block 0 to compute reverse-postorder indices (`rpo_pos`).
    // In any CFG, a reachable edge `bb -> target` is a DFS back-edge iff
    // `rpo_pos[target] <= rpo_pos[bb]`.
    let mut rpo_pos = filled(blocks, NONE, &alloc);
    let mut visited = filled(blocks, false, &alloc);
    let mut postorder: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    let mut stack: Vec<(usize, bool), A> = Vec::with_capacity_in(blocks * 2, alloc);
    stack.push((0, false));
    while let Some((bb, done)) = stack.pop() {
        if done {
            postorder.push(bb);
            continue;
        }
        if at_bool(&visited, bb) {
            continue;
        }
        set_at(&mut visited, bb, true);
        stack.push((bb, true));
        let range = view.block(Block(to_u32(bb)));
        if range.end > range.start {
            let term = range.end - 1;
            let push_start = stack.len();
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let succ = to_idx(edge.block);
                    if succ < blocks && !at_bool(&visited, succ) {
                        stack.push((succ, false));
                    }
                }
            });
            if let Some(slice) = stack.get_mut(push_start..) {
                slice.reverse();
            }
        }
    }
    let po_len = postorder.len();
    for (idx, &bb) in postorder.iter().enumerate() {
        set_at(&mut rpo_pos, bb, to_u32(po_len - 1 - idx));
    }

    // Scan all reachable terminators once for CFG back-edges replacing loop-header parameters.
    let mut is_loop_replaced_param = Vec::new_in(alloc);
    let mut head = Vec::new_in(alloc);
    let mut param: Vec<usize, A> = Vec::new_in(alloc);
    let mut next: Vec<usize, A> = Vec::new_in(alloc);
    let mut pending: Vec<(usize, usize, u32), A> = Vec::new_in(alloc);
    let mut seen_pairs: Vec<(usize, usize), A> = Vec::new_in(alloc);

    for bb in 0..blocks {
        let bb_rpo = at_u32(&rpo_pos, bb);
        if bb_rpo == NONE {
            continue;
        }
        let range = view.block(Block(to_u32(bb)));
        if range.end <= range.start {
            continue;
        }
        let term = range.end - 1;
        each(view, term, |item| {
            let Item::Edge(edge) = item else {
                return;
            };
            let target = to_idx(edge.block);
            let target_rpo = at_u32(&rpo_pos, target);
            if target_rpo == NONE || target_rpo > bb_rpo {
                return;
            }
            let Some((start, count)) = edge.args else {
                return;
            };
            let target_params = view.params(Block(edge.block));
            for k in 0..count {
                let param_vi = target_params.start + k;
                if param_vi >= target_params.end {
                    continue;
                }
                let arg_vi = to_idx(at_u32(view.extra, start + k));
                if arg_vi >= len || arg_vi == param_vi {
                    continue;
                }
                if head.is_empty() {
                    is_loop_replaced_param = filled(len, false, &alloc);
                    head = filled(len, usize::MAX, &alloc);
                }
                set_at(&mut is_loop_replaced_param, param_vi, true);
                let root_vi = record_rep_with_converts(
                    view,
                    arg_vi,
                    param_vi,
                    &mut head,
                    &mut param,
                    &mut next,
                );
                if view.op(root_vi) == Op::Param
                    && (root_vi < target_params.start || root_vi >= target_params.end)
                    && seen_pairs.len() < MAX_PARAM_PAIRS
                {
                    let root_blk = block_of_inst(view, root_vi);
                    let root_rpo = at_u32(&rpo_pos, root_blk);
                    if root_rpo != NONE && root_rpo > target_rpo {
                        let pair = (root_vi, param_vi);
                        if !seen_pairs.iter().any(|&p| p == pair) {
                            seen_pairs.push(pair);
                            pending.push((root_vi, param_vi, target_rpo));
                        }
                    }
                }
            }
        });
    }

    // Transitively propagate loop-parameter replacements backwards through intermediate in-loop
    // merge/latch `Op::Param`s (e.g., `arm -> latch(l_r2 = next_r2) -> hdr(r2 = l_r2)`).
    if !pending.is_empty() {
        let mut active_dst = filled(blocks, false, &alloc);
        let mut next_pending: Vec<(usize, usize, u32), A> = Vec::new_in(alloc);
        for _ in 0..MAX_PARAM_HOPS {
            if pending.is_empty() {
                break;
            }
            active_dst.fill(false);
            for &(merge_param_vi, _, _) in &pending {
                set_at(&mut active_dst, block_of_inst(view, merge_param_vi), true);
            }
            next_pending.clear();
            for src in 0..blocks {
                let src_rpo = at_u32(&rpo_pos, src);
                if src_rpo == NONE {
                    continue;
                }
                let range = view.block(Block(to_u32(src)));
                if range.end <= range.start {
                    continue;
                }
                let term = range.end - 1;
                each(view, term, |item| {
                    let Item::Edge(edge) = item else {
                        return;
                    };
                    let dst = to_idx(edge.block);
                    if !at_bool(&active_dst, dst) {
                        return;
                    }
                    let Some((start, count)) = edge.args else {
                        return;
                    };
                    let dst_params = view.params(Block(edge.block));
                    for &(merge_param_vi, loop_param_vi, hdr_rpo) in &pending {
                        if merge_param_vi < dst_params.start || merge_param_vi >= dst_params.end {
                            continue;
                        }
                        if src_rpo < hdr_rpo {
                            continue;
                        }
                        let k = merge_param_vi - dst_params.start;
                        if k >= count {
                            continue;
                        }
                        let arg_vi = to_idx(at_u32(view.extra, start + k));
                        if arg_vi >= len || arg_vi == loop_param_vi || arg_vi == merge_param_vi {
                            continue;
                        }
                        let root_vi = record_rep_with_converts(
                            view,
                            arg_vi,
                            loop_param_vi,
                            &mut head,
                            &mut param,
                            &mut next,
                        );
                        if view.op(root_vi) == Op::Param && seen_pairs.len() < MAX_PARAM_PAIRS {
                            let root_blk = block_of_inst(view, root_vi);
                            let root_rpo = at_u32(&rpo_pos, root_blk);
                            if root_rpo != NONE && root_rpo > hdr_rpo {
                                let pair = (root_vi, loop_param_vi);
                                if !seen_pairs.iter().any(|&p| p == pair) {
                                    seen_pairs.push(pair);
                                    next_pending.push((root_vi, loop_param_vi, hdr_rpo));
                                }
                            }
                        }
                    }
                });
            }
            core::mem::swap(&mut pending, &mut next_pending);
        }
    }

    GlobalLoopReps { is_loop_replaced_param, head, param, next }
}

fn copy_remapped<A: Allocator>(view: &View<'_>, inst: usize, val_map: &[u32], func: &mut Func<A>) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at_u32(val_map, to_idx(inst_data)),
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at_u32(view.extra, p));
                        let n_u32 = at_u32(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at_u32(view.extra, src);
                        let w = to_idx(Ty(at_u16(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at_u32(view.extra, p + k));
                            }
                            p += w;
                            func.extra.push(at_u32(view.extra, p));
                            let an_u32 = at_u32(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask | Field::Blocks => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at_u32(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Schedules pure instruction segments inside every basic block using register-pressure-aware
/// critical-path list scheduling.
pub fn schedule_dag<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<SchedOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if len < 4 {
        return Ok(SchedOutput { func: view.to_func_in(alloc), stats: SchedStats::default() });
    }

    // O(N) whole-function use summary: per SSA value, which block uses it (or `MULTI_BLOCK`),
    // its last using instruction index in that block, its same-block non-edge `Item::Value` use
    // count (excluding terminator block-parameter edge args and uses in other blocks), and whether
    // the CFG has any backedge.
    let mut use_block = filled(len, NONE, &alloc);
    let mut last_use_inst = filled(len, 0u32, &alloc);
    let mut same_block_val_uses = filled(len, 0u32, &alloc);
    let mut has_loop = false;
    for b in 0..blocks {
        let b32 = to_u32(b);
        let range = view.block(Block(b32));
        for inst in range.start..range.end {
            each(&view, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if vi >= range.start && vi < range.end {
                        if let Some(slot) = same_block_val_uses.get_mut(vi) {
                            *slot = slot.saturating_add(1);
                        }
                    }
                    record_use(v, b32, inst, &mut use_block, &mut last_use_inst);
                }
                Item::Edge(edge) => {
                    if edge.block <= b32 {
                        has_loop = true;
                    }
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            record_use(
                                at_u32(view.extra, start + k),
                                b32,
                                inst,
                                &mut use_block,
                                &mut last_use_inst,
                            );
                        }
                    }
                }
            });
        }
    }

    // Whole-function loop-carried block-parameter replacement edges (computed once per function,
    // O(V + E)) so multi-block loop bodies defer exit-only replacements even when the back-edge
    // lives in a later latch block and an earlier exit block also reads the replaced parameter.
    let global_reps = collect_global_loop_reps(&view, alloc);
    if !global_reps.head.is_empty() {
        has_loop = true;
    }

    // Whole-function backward CFG liveness over non-constant scalar GPR values (bounded by Rule 18)
    // so each segment's pressure threshold accounts for unread live-through GPRs entering the
    // segment without double-counting operands already tracked in `region_live`.
    let liveness = compute_cfg_liveness(&view, alloc);
    let mut blk_last_use = if liveness.is_some() { filled(len, 0u32, &alloc) } else { Vec::new_in(alloc) };
    let mut blk_stamp = if liveness.is_some() { filled(len, 0u32, &alloc) } else { Vec::new_in(alloc) };
    let mut blk_used: Vec<usize, A> = Vec::new_in(alloc);
    let mut cur_blk_stamp = 0u32;

    // Reusable per-segment pressure tables indexed by SSA value id (O(1) lookup, stamped reset).
    let mut seg_rem_uses = filled(len, 0u32, &alloc);
    let mut rem_non_exit_uses = filled(len, 0u16, &alloc);
    let mut seg_live_out = filled(len, false, &alloc);
    let mut seg_stamp = filled(len, 0u32, &alloc);
    let mut rep_param_stamp = filled(len, 0u32, &alloc);
    let mut cur_stamp = 0u32;

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
    let mut stats = SchedStats::default();

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let params_end = view.params(Block(b32)).end;
        let term = range.end - 1;
        let mut blk_uses_ready = false;

        for inst in range.start..params_end {
            let mapped = copy_remapped(&view, inst, &val_map, &mut func);
            set_at(&mut val_map, inst, mapped);
        }

        // Partition `params_end..term` into maximal schedulable pure segments separated by barriers.
        let mut cur = params_end;
        let mut block_reordered = false;
        while cur < term {
            if !is_schedulable_pure(view.op(cur), Flags(at_u8(view.flags, cur))) {
                let mapped = copy_remapped(&view, cur, &val_map, &mut func);
                set_at(&mut val_map, cur, mapped);
                cur += 1;
                continue;
            }
            let seg_start = cur;
            while cur < term && is_schedulable_pure(view.op(cur), Flags(at_u8(view.flags, cur))) {
                cur += 1;
            }
            let seg_end = cur;
            let seg_len = seg_end - seg_start;
            // Skip trivial or huge segments (keeps compile time strictly bounded & O(N) on huge
            // blocks; 512 covers large unrolled/SWAR hot bodies like `decode_exec_leaf_single`).
            if seg_len < 4 || seg_len > 512 {
                for inst in seg_start..seg_end {
                    let mapped = copy_remapped(&view, inst, &val_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
                continue;
            }

            // Build intra-segment SSA dependency DAG (`pred_rem[i]`, `succs[i]`, `height[i]`)
            // and register-pressure state (`region_live`, `seg_rem_uses`, `seg_live_out`).
            cur_stamp += 1;
            let mut pred_rem = filled(seg_len, 0u32, &alloc);
            let mut succs: Vec<Vec<usize, A>, A> = Vec::with_capacity_in(seg_len, alloc);
            let mut lats: Vec<u32, A> = Vec::with_capacity_in(seg_len, alloc);
            let mut is_load: Vec<bool, A> = Vec::with_capacity_in(seg_len, alloc);
            let mut intra_val_uses = filled(seg_len, 0u32, &alloc);
            let mut inst_defines_live = filled(seg_len, false, &alloc);
            let mut op_starts: Vec<usize, A> = Vec::with_capacity_in(seg_len + 1, alloc);
            let mut inst_ops: Vec<usize, A> = Vec::with_capacity_in(seg_len * 2, alloc);
            let mut region_live = 0usize;

            for i in 0..seg_len {
                let orig = seg_start + i;
                let op = view.op(orig);
                succs.push(Vec::new_in(alloc));
                lats.push(op_latency(op));
                is_load.push(op == Op::Load);
                if counts_for_live_pressure(Ty(at_u16(view.tys, orig)), op)
                    && at_u32(&use_block, orig) != NONE
                {
                    set_at(&mut inst_defines_live, i, true);
                }
            }
            for i in 0..seg_len {
                let inst = seg_start + i;
                let ops_begin = inst_ops.len();
                op_starts.push(ops_begin);
                each(&view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        if vi >= seg_start && vi < seg_end {
                            let p = vi - seg_start;
                            if let Some(slot) = intra_val_uses.get_mut(p) {
                                *slot = slot.saturating_add(1);
                            }
                            if let Some(sp) = succs.get_mut(p) {
                                if sp.scan_for(&i).is_none() {
                                    sp.push(i);
                                    if let Some(pr) = pred_rem.get_mut(i) {
                                        *pr += 1;
                                    }
                                }
                            }
                        }
                        if counts_for_live_pressure(Ty(at_u16(view.tys, vi)), view.op(vi)) {
                            let mut dup = false;
                            for idx in ops_begin..inst_ops.len() {
                                if at_usize(&inst_ops, idx) == vi {
                                    dup = true;
                                    break;
                                }
                            }
                            if !dup {
                                inst_ops.push(vi);
                                if at_u32(&seg_stamp, vi) != cur_stamp {
                                    set_at(&mut seg_stamp, vi, cur_stamp);
                                    set_at(&mut seg_rem_uses, vi, 1);
                                    set_at(&mut rem_non_exit_uses, vi, 0);
                                    let ub = at_u32(&use_block, vi);
                                    let defined_outside_block =
                                        vi < range.start || vi >= range.end;
                                    let is_lo = (has_loop && defined_outside_block)
                                        || (ub != NONE
                                            && (ub != b32
                                                || to_idx(at_u32(&last_use_inst, vi)) >= seg_end));
                                    set_at(&mut seg_live_out, vi, is_lo);
                                    if vi < seg_start || vi >= seg_end {
                                        region_live += 1;
                                    }
                                } else if let Some(slot) = seg_rem_uses.get_mut(vi) {
                                    *slot += 1;
                                }
                            }
                        }
                    }
                });
            }
            op_starts.push(inst_ops.len());

            // Identify intra-block addressing ops that AArch64 folds directly into same-block
            // `Load`/`Store` instructions (`PtrAdd(base, iconst)` or
            // `Shl(idx, 1|2|3) -> PtrAdd(base, shl) -> {Load, Store}+`, e.g. hash-table probe
            // `Load` inside this segment + update `Store` at the segment barrier immediately
            // after `seg_end`, lowered to `[base, idx, lsl #1/2/3]` with zero extra physical
            // registers). Because every folded node is consumed strictly as a `Load`/`Store`
            // address inside block `b` (never live-out of `b`), treating it as zero register
            // pressure (`+0` on def, `-0` on any kill inside the segment) keeps `region_live`
            // balanced while preventing phantom `+1/+2` spikes from blocking load hoisting.
            let mut is_folded_addr = filled(seg_len, false, &alloc);
            let is_addr_only_consumer = |cons_orig: usize, ptr_orig: usize| -> bool {
                match view.op(cons_orig) {
                    Op::Load => {
                        let cat = to_idx(at_u32(view.data, cons_orig));
                        to_idx(at_u32(view.extra, cat)) == ptr_orig
                    }
                    Op::Store => {
                        let cat = to_idx(at_u32(view.data, cons_orig));
                        to_idx(at_u32(view.extra, cat)) != ptr_orig
                            && to_idx(at_u32(view.extra, cat + 1)) == ptr_orig
                    }
                    _ => false,
                }
            };
            let is_live_out_of_block = |vi: usize| -> bool {
                match liveness.as_ref() {
                    Some(liv) => bit_get(&liv.live_out, b * liv.words, vi),
                    None => true,
                }
            };
            for p in 0..seg_len {
                let orig_p = seg_start + p;
                if view.op(orig_p) != Op::PtrAdd
                    || at_u32(&seg_stamp, orig_p) != cur_stamp
                    || is_live_out_of_block(orig_p)
                {
                    continue;
                }
                let pat = to_idx(at_u32(view.data, orig_p));
                let off = to_idx(at_u32(view.extra, pat + 1));
                if !is_folded_ptr_add_offset(&view, off) {
                    continue;
                }
                let mut saw_use = false;
                let mut all_addr_uses = true;
                if let Some(sp) = succs.get(p) {
                    for &cons in sp {
                        saw_use = true;
                        if !is_addr_only_consumer(seg_start + cons, orig_p) {
                            all_addr_uses = false;
                            break;
                        }
                    }
                }
                if all_addr_uses && at_bool(&seg_live_out, orig_p) {
                    let orig_p32 = to_u32(orig_p);
                    for post in seg_end..range.end {
                        let mut uses_p = false;
                        each(&view, post, |item| match item {
                            Item::Value(v) if v == orig_p32 => uses_p = true,
                            Item::Edge(edge) => {
                                if let Some((es, ec)) = edge.args {
                                    for k in 0..ec {
                                        if at_u32(view.extra, es + k) == orig_p32 {
                                            uses_p = true;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        });
                        if uses_p {
                            saw_use = true;
                            if !is_addr_only_consumer(post, orig_p) {
                                all_addr_uses = false;
                                break;
                            }
                        }
                    }
                }
                if saw_use && all_addr_uses {
                    set_at(&mut is_folded_addr, p, true);
                }
            }
            for s in 0..seg_len {
                let orig_s = seg_start + s;
                let rem_u = at_u32(&seg_rem_uses, orig_s);
                if !is_folded_shl_shift(&view, orig_s)
                    || at_u32(&seg_stamp, orig_s) != cur_stamp
                    || rem_u == 0
                    || rem_u != at_u32(&intra_val_uses, s)
                    || at_bool(&seg_live_out, orig_s)
                {
                    continue;
                }
                let Some(ss) = succs.get(s) else {
                    continue;
                };
                if ss.is_empty() {
                    continue;
                }
                let mut all_folded_ptr = true;
                for &cons in ss {
                    let cons_orig = seg_start + cons;
                    if view.op(cons_orig) != Op::PtrAdd || !at_bool(&is_folded_addr, cons) {
                        all_folded_ptr = false;
                        break;
                    }
                    let pat = to_idx(at_u32(view.data, cons_orig));
                    if to_idx(at_u32(view.extra, pat + 1)) != orig_s {
                        all_folded_ptr = false;
                        break;
                    }
                }
                if all_folded_ptr {
                    set_at(&mut is_folded_addr, s, true);
                }
            }

            // Count unread live-through GPR values across `seg_start..seg_end` (those defined
            // before `seg_start`, still live at/after `seg_end` or out of block `b`, and NOT
            // already counted in `region_live` via `seg_stamp == cur_stamp`).
            let mut seg_live_through = 0usize;
            if let Some(ref liv) = liveness {
                if !blk_uses_ready {
                    blk_uses_ready = true;
                    cur_blk_stamp += 1;
                    blk_used.clear();
                    for inst in range.start..range.end {
                        let inst32 = to_u32(inst);
                        each(&view, inst, |item| match item {
                            Item::Value(v) => {
                                let vi = to_idx(v);
                                if vi < len && is_gpr_live_pressure(&view, vi) {
                                    if at_u32(&blk_stamp, vi) != cur_blk_stamp {
                                        set_at(&mut blk_stamp, vi, cur_blk_stamp);
                                        blk_used.push(vi);
                                    }
                                    set_at(&mut blk_last_use, vi, inst32);
                                }
                            }
                            Item::Edge(edge) => {
                                if let Some((start, count)) = edge.args {
                                    for k in 0..count {
                                        let vi = to_idx(at_u32(view.extra, start + k));
                                        if vi < len && is_gpr_live_pressure(&view, vi) {
                                            if at_u32(&blk_stamp, vi) != cur_blk_stamp {
                                                set_at(&mut blk_stamp, vi, cur_blk_stamp);
                                                blk_used.push(vi);
                                            }
                                            set_at(&mut blk_last_use, vi, inst32);
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
                let out_base = b * liv.words;
                for w in 0..liv.words {
                    let mut word = at_u64(&liv.live_out, out_base + w);
                    while word != 0 {
                        let tz = to_idx(word.trailing_zeros());
                        let vi = (w << 6) + tz;
                        if !(vi >= seg_start && vi < range.end)
                            && at_u32(&seg_stamp, vi) != cur_stamp
                        {
                            seg_live_through += 1;
                        }
                        word &= word - 1;
                    }
                }
                let seg_end32 = to_u32(seg_end);
                for &vi in &blk_used {
                    if !(vi >= seg_start && vi < range.end)
                        && at_u32(&blk_last_use, vi) >= seg_end32
                        && at_u32(&seg_stamp, vi) != cur_stamp
                        && !bit_get(&liv.live_out, out_base, vi)
                    {
                        seg_live_through += 1;
                    }
                }
            }
            let seg_pressure_high = TOTAL_GPR_BUDGET
                .saturating_sub(seg_live_through)
                .min(REG_PRESSURE_HIGH);

            // Bottom-up critical-path heights and block-exit-only classification.
            // An instruction has `only_feeds_exit[i] == true` iff it has no non-edge `Item::Value`
            // consumers in the remainder of this block (`seg_end..=term`) AND every intra-segment
            // consumer also has `only_feeds_exit == true` (uses in other blocks, such as cold
            // bail-out stores, do NOT disqualify it!). `only_feeds_exit_or_term_icmp[i]` relaxes
            // this only for a terminator-only `Op::Icmp` feeding `Op::Br`, so loop-carried IV
            // updates (`PtrAdd`/`Add`/`Sub` feeding both `Icmp -> Br` and the back-edge) still
            // register in `rep_head` and defer until all earlier readers of the IV parameter run.
            // `is_exit_leaf[i]` marks segment-exit-only leaves (`only_feeds_exit && succs[i].is_empty()`).
            // All ops keep their true `op_latency` height so post-kill loads (`ldrh`) fire
            // immediately with full priority once the dying state is killed!
            let mut term_cond_vi = usize::MAX;
            if view.op(term) == Op::Br {
                each(&view, term, |item| {
                    if let Item::Value(v) = item {
                        term_cond_vi = to_idx(v);
                    }
                });
            }
            let mut only_feeds_exit = filled(seg_len, false, &alloc);
            let mut only_feeds_exit_or_term_icmp = filled(seg_len, false, &alloc);
            let mut is_exit_leaf = filled(seg_len, false, &alloc);
            let mut height = filled(seg_len, 0u32, &alloc);
            for i in (0..seg_len).rev() {
                let orig = seg_start + i;
                let mut max_s = 0u32;
                let has_post_seg_val_use =
                    at_u32(&same_block_val_uses, orig) > at_u32(&intra_val_uses, i);
                let mut feeds_block_val = has_post_seg_val_use;
                let is_term_only_icmp = orig == term_cond_vi
                    && view.op(orig) == Op::Icmp
                    && at_u32(&intra_val_uses, i) == 0
                    && at_u32(&same_block_val_uses, orig) == 1;
                let mut feeds_non_term_icmp = !is_term_only_icmp && has_post_seg_val_use;
                let mut has_succ = false;
                if let Some(si) = succs.get(i) {
                    has_succ = !si.is_empty();
                    for &s in si {
                        if !at_bool(&only_feeds_exit, s) {
                            feeds_block_val = true;
                        }
                        if !at_bool(&only_feeds_exit_or_term_icmp, s) {
                            feeds_non_term_icmp = true;
                        }
                        let hs = at_u32(&height, s);
                        if hs > max_s {
                            max_s = hs;
                        }
                    }
                }
                set_at(&mut only_feeds_exit, i, !feeds_block_val);
                set_at(&mut only_feeds_exit_or_term_icmp, i, !feeds_non_term_icmp);
                if !feeds_block_val && !has_succ {
                    set_at(&mut is_exit_leaf, i, true);
                }
                set_at(&mut height, i, at_u32(&lats, i) + max_s);
            }

            // Collect block parameters replaced by `only_feeds_exit` values (or terminator-only
            // IV updates `Add`/`Sub`/`PtrAdd` feeding `Icmp -> Br` and the back-edge) on the
            // block's terminator edges (walking back through zero-cost convert chains such as
            // `Load -> Zext`) so replacements wait until the replaced parameter has no other
            // remaining segment readers, then schedule immediately at full priority.
            let is_iv_update_op = |op: Op| matches!(op, Op::Add | Op::Sub | Op::PtrAdd);
            let mut is_iv_term_update = filled(seg_len, false, &alloc);
            let mut rep_head = filled(seg_len, usize::MAX, &alloc);
            let mut rep_param: Vec<usize, A> = Vec::new_in(alloc);
            let mut rep_next: Vec<usize, A> = Vec::new_in(alloc);
            each(&view, term, |item| {
                if let Item::Edge(edge) = item {
                    if let Some((start, count)) = edge.args {
                        let target_params = view.params(Block(edge.block));
                        for k in 0..count {
                            let param_vi = target_params.start + k;
                            if param_vi >= target_params.end
                                || at_u32(&seg_stamp, param_vi) != cur_stamp
                            {
                                continue;
                            }
                            let arg_vi = to_idx(at_u32(view.extra, start + k));
                            if arg_vi < seg_start || arg_vi >= seg_end {
                                continue;
                            }
                            let mut node = arg_vi - seg_start;
                            let is_exit = at_bool(&only_feeds_exit, node);
                            let is_iv_term = !is_exit
                                && at_bool(&only_feeds_exit_or_term_icmp, node)
                                && is_iv_update_op(view.op(seg_start + node));
                            if !is_exit && !is_iv_term {
                                continue;
                            }
                            if is_iv_term {
                                set_at(&mut is_iv_term_update, node, true);
                            }
                            set_at(&mut rep_param_stamp, param_vi, cur_stamp);
                            record_replaced_param(
                                node,
                                param_vi,
                                &mut rep_head,
                                &mut rep_param,
                                &mut rep_next,
                            );
                            while matches!(
                                view.op(seg_start + node),
                                Op::Zext | Op::Sext | Op::Trunc | Op::Bitcast
                            ) {
                                let mut src_vi = usize::MAX;
                                each(&view, seg_start + node, |sub| {
                                    if let Item::Value(v) = sub {
                                        src_vi = to_idx(v);
                                    }
                                });
                                if src_vi >= seg_start && src_vi < seg_start + node {
                                    let prev = src_vi - seg_start;
                                    let prev_exit = at_bool(&only_feeds_exit, prev);
                                    let prev_iv_term = !prev_exit
                                        && at_bool(&only_feeds_exit_or_term_icmp, prev)
                                        && is_iv_update_op(view.op(seg_start + prev));
                                    if prev_exit || prev_iv_term {
                                        if prev_iv_term {
                                            set_at(&mut is_iv_term_update, prev, true);
                                        }
                                        node = prev;
                                        record_replaced_param(
                                            node,
                                            param_vi,
                                            &mut rep_head,
                                            &mut rep_param,
                                            &mut rep_next,
                                        );
                                        continue;
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            });
            if !global_reps.head.is_empty() {
                for i in 0..seg_len {
                    if !at_bool(&only_feeds_exit, i)
                        && at_bool(&only_feeds_exit_or_term_icmp, i)
                        && is_iv_update_op(view.op(seg_start + i))
                        && at_head(&global_reps.head, seg_start + i) != usize::MAX
                    {
                        set_at(&mut is_iv_term_update, i, true);
                    }
                }
            }

            // Mark non-IV address-forming ops that directly feed an `Op::Load` (`PtrAdd`/`Add`),
            // or sit 1 hop before such an `Op::PtrAdd` (`PtrAdd`/`Shl`/`Zext`), so moderate-pressure
            // load hoisting unlocks address chains (e.g. `shl -> ptr_add -> load`) ahead of
            // `delta == 0` bitwise ALU ops—including loads consumed at the entry of the fallthrough
            // successor block (such as `lz77_scan_default_leaf`'s second hash-table probe).
            let mut is_load_addr = filled(seg_len, false, &alloc);
            for u in (0..seg_len).rev() {
                if at_bool(&is_iv_term_update, u) {
                    continue;
                }
                let op = view.op(seg_start + u);
                let direct_load_op = matches!(op, Op::PtrAdd | Op::Add);
                let pre_ptr_add_op = matches!(op, Op::PtrAdd | Op::Shl | Op::Zext);
                if !direct_load_op && !pre_ptr_add_op {
                    continue;
                }
                if let Some(su) = succs.get(u) {
                    for &v in su {
                        let feeds_load = direct_load_op
                            && at_bool(&is_load, v)
                            && !at_bool(&is_iv_term_update, v);
                        let feeds_load_ptr_add = pre_ptr_add_op
                            && view.op(seg_start + v) == Op::PtrAdd
                            && at_bool(&is_load_addr, v);
                        if feeds_load || feeds_load_ptr_add {
                            set_at(&mut is_load_addr, u, true);
                            break;
                        }
                    }
                }
            }

            // Record each `Op::Select`'s underlying `Op::Icmp` operand pair so sibling selects
            // sharing the same comparison (`(lhs, rhs)`) schedule contiguously for zero-`cset`
            // shared-NZCV fusion in `isel`, and `x == 0`/`x != 0` selects fire before non-zero
            // `(x, k)` select families so they reuse `subs`/`adds` flags without splitting `(x, k)`.
            let mut select_fam = filled(seg_len, None::<(u32, u32)>, &alloc);
            let mut select_is_zero_eq = filled(seg_len, false, &alloc);
            for i in 0..seg_len {
                if let Some((a, b, is_zero_eq)) = select_cmp_info(&view, seg_start + i) {
                    set_at(&mut select_fam, i, Some((a, b)));
                    set_at(&mut select_is_zero_eq, i, is_zero_eq);
                } else if is_zero_eq_icmp(&view, seg_start + i) {
                    set_at(&mut select_is_zero_eq, i, true);
                }
            }
            let mut active_cmp_family: Option<(u32, u32)> = None;

            // Count unscheduled `!only_feeds_exit` segment readers per SSA value, excluding
            // terminator-only IV updates (which replace a live parameter and defer until all
            // other readers of that parameter finish, so they must not block those readers).
            for i in 0..seg_len {
                if !at_bool(&only_feeds_exit, i) && !at_bool(&is_iv_term_update, i) {
                    let op_s = at_usize(&op_starts, i);
                    let op_e = at_usize(&op_starts, i + 1);
                    for idx in op_s..op_e {
                        let vi = at_usize(&inst_ops, idx);
                        if let Some(slot) = rem_non_exit_uses.get_mut(vi) {
                            *slot = slot.saturating_add(1);
                        }
                    }
                }
            }

            let mut ready_at = filled(seg_len, 0u32, &alloc);
            let mut ready: Vec<usize, A> = Vec::with_capacity_in(seg_len, alloc);
            for i in 0..seg_len {
                if at_u32(&pred_rem, i) == 0 {
                    ready.push(i);
                }
            }

            let mut cycle = 0u32;
            let mut emitted_pos = 0usize;
            while !ready.is_empty() {
                let high_pressure = region_live >= seg_pressure_high;
                let total_live = region_live.saturating_add(seg_live_through);
                let defer_exit_alu_active = total_live >= EXIT_ALU_DEFER_PRESSURE;
                let allow_load_hoist = total_live < LOAD_HOIST_PRESSURE_LIMIT;
                let is_deferred_exit_alu = |n: usize, delta: i32| -> bool {
                    defer_exit_alu_active
                        && delta > 0
                        && at_bool(&only_feeds_exit, n)
                        && !at_bool(&is_load, n)
                        && !(allow_load_hoist && at_bool(&is_load_addr, n))
                };
                let load_urgency = |n: usize| -> u8 {
                    if !allow_load_hoist {
                        return 0;
                    }
                    if at_bool(&is_load, n) && !at_bool(&is_iv_term_update, n) {
                        2
                    } else if at_bool(&is_load_addr, n) {
                        1
                    } else {
                        0
                    }
                };
                let kill_ctx = KillCtx {
                    seg_start,
                    only_feeds_exit: &only_feeds_exit,
                    is_iv_term_update: &is_iv_term_update,
                    is_exit_leaf: &is_exit_leaf,
                    is_load: &is_load,
                    op_starts: &op_starts,
                    inst_ops: &inst_ops,
                    rem_non_exit_uses: &rem_non_exit_uses,
                    seg_rem_uses: &seg_rem_uses,
                    seg_stamp: &seg_stamp,
                    seg_live_out: &seg_live_out,
                    rep_param_stamp: &rep_param_stamp,
                    is_loop_replaced_param: &global_reps.is_loop_replaced_param,
                    cur_stamp,
                    rep_head: &rep_head,
                    rep_param: &rep_param,
                    rep_next: &rep_next,
                    global_rep_head: &global_reps.head,
                    global_rep_param: &global_reps.param,
                    global_rep_next: &global_reps.next,
                };
                let fam_match = |n: usize| -> bool {
                    active_cmp_family.is_some()
                        && select_fam.get(n).copied().flatten() == active_cmp_family
                };
                let mut best_idx = 0usize;
                let first_node = at_usize(&ready, 0);
                let mut best_delta = calc_live_delta(
                    &view,
                    first_node,
                    seg_start,
                    seg_end,
                    &inst_defines_live,
                    &is_folded_addr,
                    &op_starts,
                    &inst_ops,
                    &seg_rem_uses,
                    &seg_live_out,
                );
                let mut best_blocks_kill = kill_ctx.blocks_operand_kill(first_node);
                let mut best_defer_exit_alu = is_deferred_exit_alu(first_node, best_delta);
                let mut best_load_urgency = load_urgency(first_node);
                let mut best_fam_match = fam_match(first_node);
                let mut best_zero_eq = at_bool(&select_is_zero_eq, first_node);
                for k in 1..ready.len() {
                    let a = at_usize(&ready, k);
                    let b_node = at_usize(&ready, best_idx);
                    let da = calc_live_delta(
                        &view,
                        a,
                        seg_start,
                        seg_end,
                        &inst_defines_live,
                        &is_folded_addr,
                        &op_starts,
                        &inst_ops,
                        &seg_rem_uses,
                        &seg_live_out,
                    );
                    let db = best_delta;
                    let ka = kill_ctx.blocks_operand_kill(a);
                    let kb = best_blocks_kill;
                    let ea = is_deferred_exit_alu(a, da);
                    let eb = best_defer_exit_alu;
                    let la = load_urgency(a);
                    let lb = best_load_urgency;
                    let fa = fam_match(a);
                    let fb = best_fam_match;
                    let za = at_bool(&select_is_zero_eq, a);
                    let zb = best_zero_eq;
                    let ha = at_u32(&height, a);
                    let hb = at_u32(&height, b_node);
                    let better = if high_pressure {
                        // High register pressure: keep sibling `Op::Select`s on the active `Icmp`
                        // family strictly contiguous (avoiding `cset` + duplicate `cmp`!), then
                        // defer exit-only ops blocking an operand/replaced-param kill, then defer
                        // pressure-increasing (`delta > 0`) non-`Load` exit-only ALU ops when
                        // total live pressure >= `EXIT_ALU_DEFER_PRESSURE`, then rank strictly
                        // draining ops (`delta < 0`) first, then urgent non-exit `Op::Load` /
                        // load-addr precursors, then zero-eq `Op::Select`s, then remaining
                        // `delta >= 0` ops by smallest `live_delta`, breaking ties by highest
                        // `height`, then original index.
                        let ra = high_pressure_rank(da, la, za);
                        let rb = high_pressure_rank(db, lb, zb);
                        if fa != fb {
                            fa
                        } else if ka != kb {
                            !ka
                        } else if ea != eb {
                            !ea
                        } else if ra != rb {
                            ra < rb
                        } else if da != db {
                            da < db
                        } else if ha != hb {
                            ha > hb
                        } else {
                            a < b_node
                        }
                    } else {
                        // Low register pressure: keep sibling `Op::Select`s on the active `Icmp`
                        // family contiguous, then defer exit-only ops blocking an operand/param
                        // kill, then defer pressure-increasing (`delta > 0`) non-`Load` exit-only
                        // ALU ops, then prefer ready-now (`ready_at <= cycle`), highest
                        // critical-path `height`, zero-eq `Select`, smallest `live_delta`,
                        // earliest `ready_at`, then original index.
                        let ra = at_u32(&ready_at, a);
                        let rb = at_u32(&ready_at, b_node);
                        let a_now = ra <= cycle;
                        let b_now = rb <= cycle;
                        if fa != fb {
                            fa
                        } else if ka != kb {
                            !ka
                        } else if ea != eb {
                            !ea
                        } else if a_now != b_now {
                            a_now
                        } else if ha != hb {
                            ha > hb
                        } else if za != zb {
                            za
                        } else if da != db {
                            da < db
                        } else if ra != rb {
                            ra < rb
                        } else {
                            a < b_node
                        }
                    };
                    if better {
                        best_idx = k;
                        best_delta = da;
                        best_blocks_kill = ka;
                        best_defer_exit_alu = ea;
                        best_load_urgency = la;
                        best_fam_match = fa;
                        best_zero_eq = za;
                    }
                }
                let pick = ready.remove_swap(best_idx);
                if pick != emitted_pos {
                    block_reordered = true;
                    stats.insts_reordered += 1;
                }
                emitted_pos += 1;
                let orig = seg_start + pick;
                if let Some(fam) = select_fam.get(pick).copied().flatten() {
                    active_cmp_family = Some(fam);
                } else if matches!(view.op(orig), Op::Select | Op::Icmp | Op::Fcmp) {
                    active_cmp_family = None;
                }
                let mapped = copy_remapped(&view, orig, &val_map, &mut func);
                set_at(&mut val_map, orig, mapped);

                // Update `region_live`, `rem_non_exit_uses`, and `seg_rem_uses` for the scheduled
                // instruction.
                if at_bool(&inst_defines_live, pick) && !at_bool(&is_folded_addr, pick) {
                    region_live += 1;
                }
                let pick_non_exit =
                    !at_bool(&only_feeds_exit, pick) && !at_bool(&is_iv_term_update, pick);
                let op_s = at_usize(&op_starts, pick);
                let op_e = at_usize(&op_starts, pick + 1);
                for idx in op_s..op_e {
                    let vi = at_usize(&inst_ops, idx);
                    if pick_non_exit {
                        if let Some(rem_ne) = rem_non_exit_uses.get_mut(vi) {
                            *rem_ne = rem_ne.saturating_sub(1);
                        }
                    }
                    if let Some(rem) = seg_rem_uses.get_mut(vi) {
                        if *rem > 0 {
                            *rem -= 1;
                            if *rem == 0
                                && !at_bool(&seg_live_out, vi)
                                && !is_folded_addr_inst(vi, seg_start, seg_end, &is_folded_addr)
                                && counts_for_live_pressure(Ty(at_u16(view.tys, vi)), view.op(vi))
                            {
                                region_live = region_live.saturating_sub(1);
                            }
                        }
                    }
                }

                let finish = cycle.max(at_u32(&ready_at, pick)) + at_u32(&lats, pick);
                if let Some(sp) = succs.get(pick) {
                    for &s in sp {
                        if let Some(rs) = ready_at.get_mut(s) {
                            if finish > *rs {
                                *rs = finish;
                            }
                        }
                        if let Some(pr) = pred_rem.get_mut(s) {
                            *pr -= 1;
                            if *pr == 0 {
                                ready.push(s);
                            }
                        }
                    }
                }
                cycle += 1;
            }
        }
        if block_reordered {
            stats.blocks_scheduled += 1;
        }
        let mapped = copy_remapped(&view, term, &val_map, &mut func);
        set_at(&mut val_map, term, mapped);
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(SchedOutput { func, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv},
    };

    #[test]
    fn schedule_dag_interleaves_independent_mul_chains_and_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let e = b.create_block(BlockKind::Normal);
        b.begin(e);
        let a = b.param(Ty::I64);
        let x = b.param(Ty::I64);
        let c = b.param(Ty::I64);
        let d = b.param(Ty::I64);
        // Chain 1 back-to-back, then Chain 2 back-to-back: scheduler should interleave them!
        let m1 = b.binary(Op::Mul, Flags::NONE, a, x);
        let m2 = b.binary(Op::Mul, Flags::NONE, m1, a);
        let m3 = b.binary(Op::Add, Flags::NONE, m2, x);
        let n1 = b.binary(Op::Mul, Flags::NONE, c, d);
        let n2 = b.binary(Op::Mul, Flags::NONE, n1, c);
        let n3 = b.binary(Op::Add, Flags::NONE, n2, d);
        let out = b.binary(Op::Xor, Flags::NONE, m3, n3);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        assert!(sched.stats.blocks_scheduled >= 1);
        assert!(sched.stats.insts_reordered >= 1);
        // Verify first two ops in scheduled block are BOTH independent `Op::Mul`s (interleaved)!
        assert_eq!(
            [sched.func.view().op(4), sched.func.view().op(5)],
            [Op::Mul, Op::Mul],
            "expected scheduler to start both independent Mul chains first!"
        );

        let args = [Val::Int(7), Val::Int(13), Val::Int(19), Val::Int(23)];
        let Outcome::Returned(r0) = eval::run_in(func.view(), &args, 1000, TestHeap).unwrap() else { panic!() };
        let Outcome::Returned(r1) = eval::run_in(sched.func.view(), &args, 1000, TestHeap).unwrap() else { panic!() };
        assert_eq!(r0, r1);
    }

    fn peak_live_in_block(view: View<'_>, block: Block) -> usize {
        let len = view.len();
        let mut rem_uses = filled(len, 0u32, &TestHeap);
        let mut inst_ops: Vec<Vec<usize, TestHeap>, TestHeap> = Vec::with_capacity_in(len, TestHeap);
        for inst in 0..len {
            let mut ops: Vec<usize, TestHeap> = Vec::new_in(TestHeap);
            each(&view, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if ops.scan_for(&vi).is_none() {
                        ops.push(vi);
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            *slot += 1;
                        }
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = to_idx(at_u32(view.extra, start + k));
                            if ops.scan_for(&vi).is_none() {
                                ops.push(vi);
                                if let Some(slot) = rem_uses.get_mut(vi) {
                                    *slot += 1;
                                }
                            }
                        }
                    }
                }
            });
            inst_ops.push(ops);
        }

        let range = view.block(block);
        let mut live = 0usize;
        let mut peak = 0usize;
        for inst in range.start..range.end {
            let ty = Ty(at_u16(view.tys, inst));
            if counts_for_pressure(ty) && at_u32(&rem_uses, inst) > 0 {
                live += 1;
            }
            if let Some(ops) = inst_ops.get(inst) {
                for &vi in ops {
                    if counts_for_pressure(Ty(at_u16(view.tys, vi))) {
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            if *slot > 0 {
                                *slot -= 1;
                                if *slot == 0 {
                                    live = live.saturating_sub(1);
                                }
                            }
                        }
                    }
                }
            }
            if live > peak {
                peak = live;
            }
        }
        peak
    }

    #[test]
    fn schedule_dag_bounds_register_pressure_on_wide_chains() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let e = b.create_block(BlockKind::Normal);
        b.begin(e);
        let p0 = b.param(Ty::I64);
        let p1 = b.param(Ty::I64);
        let p2 = b.param(Ty::I64);
        let p3 = b.param(Ty::I64);
        let p4 = b.param(Ty::I64);
        let p5 = b.param(Ty::I64);

        // 3 independent wide chains (8 leaves each = 24 concurrent values > 22 under pure ILP
        // interleaving, each leaf squared via `Mul` and reduced through an Add/Xor tree).
        let mut build_chain = |x, y| {
            let d0 = b.binary(Op::Add, Flags::NONE, x, y);
            let d1 = b.binary(Op::Sub, Flags::NONE, x, y);
            let d2 = b.binary(Op::Xor, Flags::NONE, x, y);
            let d3 = b.binary(Op::Or, Flags::NONE, x, y);
            let d4 = b.binary(Op::And, Flags::NONE, x, y);
            let d5 = b.binary(Op::Sub, Flags::NONE, y, x);
            let d6 = b.binary(Op::Add, Flags::NONE, x, x);
            let d7 = b.binary(Op::Add, Flags::NONE, y, y);

            let m0 = b.binary(Op::Mul, Flags::NONE, d0, d0);
            let m1 = b.binary(Op::Mul, Flags::NONE, d1, d1);
            let m2 = b.binary(Op::Mul, Flags::NONE, d2, d2);
            let m3 = b.binary(Op::Mul, Flags::NONE, d3, d3);
            let m4 = b.binary(Op::Mul, Flags::NONE, d4, d4);
            let m5 = b.binary(Op::Mul, Flags::NONE, d5, d5);
            let m6 = b.binary(Op::Mul, Flags::NONE, d6, d6);
            let m7 = b.binary(Op::Mul, Flags::NONE, d7, d7);

            let r0 = b.binary(Op::Add, Flags::NONE, m0, m1);
            let r1 = b.binary(Op::Add, Flags::NONE, m2, m3);
            let r2 = b.binary(Op::Add, Flags::NONE, m4, m5);
            let r3 = b.binary(Op::Add, Flags::NONE, m6, m7);
            let s0 = b.binary(Op::Xor, Flags::NONE, r0, r1);
            let s1 = b.binary(Op::Xor, Flags::NONE, r2, r3);
            b.binary(Op::Add, Flags::NONE, s0, s1)
        };

        let c0 = build_chain(p0, p1);
        let c1 = build_chain(p2, p3);
        let c2 = build_chain(p4, p5);
        let c01 = b.binary(Op::Xor, Flags::NONE, c0, c1);
        let out = b.binary(Op::Xor, Flags::NONE, c01, c2);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        assert!(sched.stats.blocks_scheduled >= 1);

        let peak = peak_live_in_block(sched.func.view(), Block(0));
        assert!(
            peak <= REG_PRESSURE_HIGH + 1,
            "expected pressure-aware schedule_dag to bound peak live <= {} (pure ILP peaks at 24), got {peak}",
            REG_PRESSURE_HIGH + 1,
        );

        let args = [
            Val::Int(3),
            Val::Int(5),
            Val::Int(7),
            Val::Int(11),
            Val::Int(13),
            Val::Int(17),
        ];
        let Outcome::Returned(r0) = eval::run_in(func.view(), &args, 1000, TestHeap).unwrap() else {
            panic!()
        };
        let Outcome::Returned(r1) = eval::run_in(sched.func.view(), &args, 1000, TestHeap).unwrap() else {
            panic!()
        };
        assert_eq!(r0, r1);
    }

    #[test]
    fn schedule_dag_drains_dying_temps_under_high_pressure() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let e = b.create_block(BlockKind::Normal);
        b.begin(e);
        let p0 = b.param(Ty::I64);
        let p1 = b.param(Ty::I64);
        let p2 = b.param(Ty::I64);
        let p3 = b.param(Ty::I64);
        let p4 = b.param(Ty::I64);
        let p5 = b.param(Ty::I64);
        let p6 = b.param(Ty::I64);
        let p7 = b.param(Ty::I64);

        // Push intra-segment live count to REG_PRESSURE_HIGH (12) with 8 live params + 4 locals.
        let e_ml = b.binary(Op::Add, Flags::NONE, p0, p1);
        let t1 = b.binary(Op::Sub, Flags::NONE, p2, p3);
        let t2 = b.binary(Op::Xor, Flags::NONE, p4, p5);
        let t3 = b.binary(Op::Or, Flags::NONE, p6, p7);

        // Multi-use local temporary `e_ml` (2 consumers -> 1 reducing consumer `net_delta = -1`).
        let ml_base = b.binary(Op::Add, Flags::NONE, e_ml, p0);
        let ml_extra = b.binary(Op::Xor, Flags::NONE, e_ml, p1);
        let match_len = b.binary(Op::Add, Flags::NONE, ml_base, ml_extra);

        // Taller unrelated chain on `t1..t3` (higher critical-path height than `match_len`).
        let r0 = b.binary(Op::Mul, Flags::NONE, t1, t2);
        let r1 = b.binary(Op::Mul, Flags::NONE, r0, t3);
        let r2 = b.binary(Op::Mul, Flags::NONE, r1, p2);
        let r3 = b.binary(Op::Mul, Flags::NONE, r2, p3);
        let r4 = b.binary(Op::Mul, Flags::NONE, r3, p4);
        let r5 = b.binary(Op::Mul, Flags::NONE, r4, p5);
        let r6 = b.binary(Op::Add, Flags::NONE, r5, p6);
        let r7 = b.binary(Op::Xor, Flags::NONE, r6, p7);

        let out = b.binary(Op::Xor, Flags::NONE, match_len, r7);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let peak = peak_live_in_block(sched.func.view(), Block(0));
        assert!(
            peak <= REG_PRESSURE_HIGH + 1,
            "expected high-pressure draining to keep peak live <= {}, got {peak}",
            REG_PRESSURE_HIGH + 1,
        );

        let args = [
            Val::Int(3),
            Val::Int(5),
            Val::Int(7),
            Val::Int(11),
            Val::Int(13),
            Val::Int(17),
            Val::Int(19),
            Val::Int(23),
        ];
        let Outcome::Returned(r0) = eval::run_in(func.view(), &args, 1000, TestHeap).unwrap() else {
            panic!()
        };
        let Outcome::Returned(r1) = eval::run_in(sched.func.view(), &args, 1000, TestHeap).unwrap() else {
            panic!()
        };
        assert_eq!(r0, r1);
    }

    #[test]
    fn schedule_dag_does_not_hoist_backedge_load_above_dying_state_alu() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // 1. Loop body matching `fse_pack_sequences_leaf`: `raw_next = load i16` -> `zext i64`
        //    feeds ONLY the loop back-edge param, while `of_st_bits = and(of_state, mask)` is
        //    the dying use of `of_state`. Even when `load` appears BEFORE `of_st_bits` in input
        //    IR, `schedule_dag` must emit `of_st_bits` BEFORE `load` so `of_state` dies first.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let tbl_slot = b.slot(8 * 2, 1, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let init_state = b.param(Ty::I64);
        let mask = b.param(Ty::I64);
        let tbl = b.slot_addr(tbl_slot);
        let table_vals: [u16; 8] = [3, 5, 1, 7, 2, 6, 0, 4];
        for (k, &tv) in table_vals.iter().enumerate() {
            let v = b.iconst(Ty::I16, u128::from(tv));
            let off = i32::try_from(k * 2).unwrap();
            b.store(Flags::NONE, v, tbl, off, MemAttrs::aligned(1));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let seven = b.iconst(Ty::I64, 7);
        b.jump((loop_blk, &[init_state, zero, n]));

        b.begin(loop_blk);
        let of_state = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);
        // Back-edge load chain placed FIRST in input IR:
        let idx = b.binary(Op::And, Flags::NONE, of_state, seven);
        let byte_off = b.binary(Op::Shl, Flags::NONE, idx, one);
        let elem_ptr = b.ptr_add(Flags::NONE, tbl, byte_off);
        let raw_next = b.load(Ty::I16, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(1));
        let next_of = b.convert(Op::Zext, Flags::NONE, Ty::I64, raw_next);
        // Dying-state ALU op killing `of_state`:
        let of_st_bits = b.binary(Op::And, Flags::NONE, of_state, mask);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, of_st_bits);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, one);
        let cont = b.icmp(IntCond::Ne, next_rem, zero);
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, &[next_of, next_acc, next_rem]),
            (exit_blk, &[next_acc]),
        );

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let mut last_and_pos = None;
        let mut load_pos = None;
        for inst in loop_range {
            match sv.op(inst) {
                Op::And => last_and_pos = Some(inst),
                Op::Load => load_pos = Some(inst),
                _ => {}
            }
        }
        let last_and = last_and_pos.unwrap();
        let load_inst = load_pos.unwrap();
        assert!(
            last_and < load_inst,
            "expected dying-state Op::And ({last_and}) to run BEFORE back-edge-only Op::Load ({load_inst})"
        );

        for &(nv, st, mk) in &[(1u64, 0u64, 0xffu64), (5, 2, 0x3), (12, 7, 0x5)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(st)), Val::Int(u128::from(mk))];
            let Outcome::Returned(r0) = eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            let Outcome::Returned(r1) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            assert_eq!(r0, r1, "backedge load schedule mismatch on ({nv}, {st}, {mk})");
        }

        // 2. Soundness / negative check: a `Load` feeding `Br` condition MUST STILL be hoisted
        //    ahead of independent lower-height ALU ops!
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let val_slot = b2.slot(8, 3, SlotKind::Temp);
        let [e2, out_blk] = [(); 2].map(|_| b2.create_block(BlockKind::Normal));

        b2.begin(e2);
        let p0 = b2.param(Ty::I64);
        let p1 = b2.param(Ty::I64);
        let slot_p = b2.slot_addr(val_slot);
        b2.store(Flags::NONE, p0, slot_p, 0, MemAttrs::aligned(3));
        let z2 = b2.iconst(Ty::I64, 0);
        // Low-height ALU placed before a `Br`-cond-feeding Load:
        let s0 = b2.binary(Op::Add, Flags::NONE, p0, p1);
        let s1 = b2.binary(Op::Xor, Flags::NONE, s0, p1);
        let loaded = b2.load(Ty::I64, Flags::NONE, slot_p, 0, MemAttrs::aligned(3));
        let cond2 = b2.icmp(IntCond::Ne, loaded, z2);
        b2.br(Flags::NONE, cond2, (out_blk, &[s1]), (out_blk, &[p0]));

        b2.begin(out_blk);
        let r_out = b2.param(Ty::I64);
        b2.ret(&[r_out]);
        let func2 = b2.finish_checked().unwrap();

        let sched2 = schedule_dag(func2.view(), TestHeap).unwrap();
        let sv2 = sched2.func.view();
        verify(sv2).unwrap();
        let mut add_pos2 = None;
        let mut load_pos2 = None;
        for inst in sv2.block(Block(0)) {
            match sv2.op(inst) {
                Op::Add => add_pos2 = Some(inst),
                Op::Load => load_pos2 = Some(inst),
                _ => {}
            }
        }
        assert!(
            load_pos2.unwrap() < add_pos2.unwrap(),
            "expected Br-cond-feeding Op::Load to still be hoisted above independent Op::Add"
        );
    }

    #[test]
    fn schedule_dag_defers_edge_only_select_until_non_edge_kills_shared_operand() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `decode_exec_leaf_single`: `next_r2 = select(rc < 2, r2, r1)` feeds ONLY the
        // loop back-edge (`edge_only`), while `rep_hi = select(rc == 2, r2, r0_m2)` ALSO reads
        // `r2` and feeds non-edge work (`Add` -> `Store`). Even though `next_r2` appears first
        // in input IR and becomes ready-now before `rep_hi` (delayed behind a 2-mul chain),
        // `schedule_dag` must schedule `rep_hi` BEFORE `next_r2` so `r2` dies first.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let rc_in = b.param(Ty::I64);
        let r0_in = b.param(Ty::I64);
        let r1_in = b.param(Ty::I64);
        let r2_in = b.param(Ty::I64);
        let out_p = b.slot_addr(out_slot);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        b.jump((loop_blk, &[rc_in, r0_in, r1_in, r2_in, c0, n]));

        b.begin(loop_blk);
        let rc = b.param(Ty::I64);
        let r0 = b.param(Ty::I64);
        let r1 = b.param(Ty::I64);
        let r2 = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);

        // Edge-only `Select` placed FIRST and ready quickly:
        let cmp_lt2 = b.icmp(IntCond::Ult, rc, c2);
        let next_r2 = b.select(Flags::NONE, cmp_lt2, r2, r1);
        // Non-edge `Select` reading `r2`, gated behind a higher-latency `Mul` chain:
        let r0_m1 = b.binary(Op::Mul, Flags::NONE, r0, c3);
        let r0_m2 = b.binary(Op::Mul, Flags::NONE, r0_m1, r0);
        let cmp_eq2 = b.icmp(IntCond::Eq, rc, c2);
        let rep_hi = b.select(Flags::NONE, cmp_eq2, r2, r0_m2);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, rep_hi);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.store(Flags::NONE, next_acc, out_p, 0, MemAttrs::aligned(3));
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, &[rc, r0, r1, next_r2, next_acc, next_rem]),
            (exit_blk, &[next_acc]),
        );

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let mut val_uses = filled(sv.len(), 0u32, &TestHeap);
        for inst in 0..sv.len() {
            each(&sv, inst, |item| {
                if let Item::Value(v) = item {
                    if let Some(slot) = val_uses.get_mut(to_idx(v)) {
                        *slot = slot.saturating_add(1);
                    }
                }
            });
        }

        let mut non_edge_sel = None;
        let mut edge_only_sel = None;
        for inst in sv.block(Block(1)) {
            if sv.op(inst) == Op::Select {
                if at_u32(&val_uses, inst) > 0 {
                    non_edge_sel = Some(inst);
                } else {
                    edge_only_sel = Some(inst);
                }
            }
        }
        let non_edge_pos = non_edge_sel.unwrap();
        let edge_only_pos = edge_only_sel.unwrap();
        assert!(
            non_edge_pos < edge_only_pos,
            "expected non-edge Op::Select ({non_edge_pos}) killing `r2` to run BEFORE edge-only Op::Select ({edge_only_pos})"
        );

        for &rc_v in &[0u64, 1, 2, 3] {
            let args = [
                Val::Int(4),
                Val::Int(u128::from(rc_v)),
                Val::Int(11),
                Val::Int(29),
                Val::Int(47),
            ];
            let Outcome::Returned(r_before) = eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            assert_eq!(r_before, r_after, "edge-only select deferral mismatch on rc={rc_v}");
        }
    }

    #[test]
    fn schedule_dag_defers_liveout_select_with_cold_exit_store_and_hoists_load_after_kill() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // 1. Mirrors `decode_exec_leaf_single`: `next_r2 = select(rc < 2, r2, r1)` is passed on
        //    the hot loop back-edge AND directly stored to `st.rep[2]` in a cold bail-out block
        //    (so `total_val_uses[next_r2] > 0`, i.e. NOT globally edge-only!). Inside the loop
        //    block it is strictly live-out (`only_feeds_exit == true`), so `schedule_dag` must
        //    still defer `next_r2` until `rep_hi` kills `r2`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let bail_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_blk, bail_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let rc_in = b.param(Ty::I64);
        let r0_in = b.param(Ty::I64);
        let r1_in = b.param(Ty::I64);
        let r2_in = b.param(Ty::I64);
        let out_p = b.slot_addr(out_slot);
        let bail_p = b.slot_addr(bail_slot);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        b.jump((loop_blk, &[rc_in, r0_in, r1_in, r2_in, c0, n]));

        b.begin(loop_blk);
        let rc = b.param(Ty::I64);
        let r0 = b.param(Ty::I64);
        let r1 = b.param(Ty::I64);
        let r2 = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);

        let cmp_lt2 = b.icmp(IntCond::Ult, rc, c2);
        let next_r2 = b.select(Flags::NONE, cmp_lt2, r2, r1);
        let r0_m1 = b.binary(Op::Mul, Flags::NONE, r0, c3);
        let r0_m2 = b.binary(Op::Mul, Flags::NONE, r0_m1, r0);
        let cmp_eq2 = b.icmp(IntCond::Eq, rc, c2);
        let rep_hi = b.select(Flags::NONE, cmp_eq2, r2, r0_m2);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, rep_hi);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.store(Flags::NONE, next_acc, out_p, 0, MemAttrs::aligned(3));
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, &[rc, r0, r1, next_r2, next_acc, next_rem]),
            (bail_blk, &[next_acc]),
        );

        b.begin(bail_blk);
        let res = b.param(Ty::I64);
        b.store(Flags::NONE, next_r2, bail_p, 0, MemAttrs::aligned(3));
        let saved = b.load(Ty::I64, Flags::NONE, bail_p, 0, MemAttrs::aligned(3));
        let final_res = b.binary(Op::Add, Flags::NONE, res, saved);
        b.ret(&[final_res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let mut same_blk_uses = filled(sv.len(), 0u32, &TestHeap);
        for inst in loop_range.clone() {
            each(&sv, inst, |item| {
                if let Item::Value(v) = item {
                    if let Some(slot) = same_blk_uses.get_mut(to_idx(v)) {
                        *slot = slot.saturating_add(1);
                    }
                }
            });
        }
        let mut rep_hi_sel = None;
        let mut next_r2_sel = None;
        for inst in loop_range {
            if sv.op(inst) == Op::Select {
                if at_u32(&same_blk_uses, inst) > 0 {
                    rep_hi_sel = Some(inst);
                } else {
                    next_r2_sel = Some(inst);
                }
            }
        }
        let rep_hi_pos = rep_hi_sel.unwrap();
        let next_r2_pos = next_r2_sel.unwrap();
        assert!(
            rep_hi_pos < next_r2_pos,
            "expected rep_hi Op::Select ({rep_hi_pos}) killing `r2` to run BEFORE live-out-only next_r2 Op::Select ({next_r2_pos}) even with cold bail store"
        );

        for &rc_v in &[0u64, 1, 2, 3] {
            let args = [
                Val::Int(4),
                Val::Int(u128::from(rc_v)),
                Val::Int(11),
                Val::Int(29),
                Val::Int(47),
            ];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(r_before, r_after, "cold-bail select deferral mismatch on rc={rc_v}");
        }

        // 2. Mirrors `fse_pack_sequences_leaf`: a back-edge `Load` (`next_st`) replacing `st`
        //    must wait until `and(st, mask)` kills `st`, and the instant `st` is dead it must
        //    schedule at full load priority BEFORE unrelated low-height ALU (`u1 = xor(x, 7)`).
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let tbl_slot = b2.slot(8, 3, SlotKind::Temp);
        let out_slot2 = b2.slot(8, 3, SlotKind::Temp);
        let [entry2, loop_blk2, exit_blk2] = [(); 3].map(|_| b2.create_block(BlockKind::Normal));

        b2.begin(entry2);
        let n2 = b2.param(Ty::I64);
        let init_st = b2.param(Ty::I64);
        let mask2 = b2.param(Ty::I64);
        let x_in = b2.param(Ty::I64);
        let tbl_p = b2.slot_addr(tbl_slot);
        let out_p2 = b2.slot_addr(out_slot2);
        let c42 = b2.iconst(Ty::I64, 42);
        b2.store(Flags::NONE, c42, tbl_p, 0, MemAttrs::aligned(3));
        let z0 = b2.iconst(Ty::I64, 0);
        let z1 = b2.iconst(Ty::I64, 1);
        let z7 = b2.iconst(Ty::I64, 7);
        b2.jump((loop_blk2, &[init_st, z0, x_in, n2]));

        b2.begin(loop_blk2);
        let st = b2.param(Ty::I64);
        let acc2 = b2.param(Ty::I64);
        let x = b2.param(Ty::I64);
        let rem2 = b2.param(Ty::I64);

        let next_st = b2.load(Ty::I64, Flags::NONE, tbl_p, 0, MemAttrs::aligned(3));
        let u1 = b2.binary(Op::Xor, Flags::NONE, x, z7);
        let st_bits = b2.binary(Op::And, Flags::NONE, st, mask2);
        let st_add = b2.binary(Op::Add, Flags::NONE, st_bits, z7);
        let next_acc2 = b2.binary(Op::Add, Flags::NONE, acc2, st_add);
        let u2 = b2.binary(Op::Or, Flags::NONE, next_acc2, u1);
        let next_rem2 = b2.binary(Op::Sub, Flags::NONE, rem2, z1);
        let cont2 = b2.icmp(IntCond::Ne, next_rem2, z0);
        b2.store(Flags::NONE, u2, out_p2, 0, MemAttrs::aligned(3));
        b2.br(
            Flags::NONE,
            cont2,
            (loop_blk2, &[next_st, next_acc2, u1, next_rem2]),
            (exit_blk2, &[u2]),
        );

        b2.begin(exit_blk2);
        let res2 = b2.param(Ty::I64);
        b2.ret(&[res2]);
        let func2 = b2.finish_checked().unwrap();

        let sched2 = schedule_dag(func2.view(), TestHeap).unwrap();
        let sv2 = sched2.func.view();
        verify(sv2).unwrap();

        let mut and_pos = None;
        let mut load_pos = None;
        let mut xor_pos = None;
        for inst in sv2.block(Block(1)) {
            match sv2.op(inst) {
                Op::And => and_pos = Some(inst),
                Op::Load => load_pos = Some(inst),
                Op::Xor => xor_pos = Some(inst),
                _ => {}
            }
        }
        let and_i = and_pos.unwrap();
        let load_i = load_pos.unwrap();
        let xor_i = xor_pos.unwrap();
        assert!(
            and_i < load_i,
            "expected dying-state Op::And ({and_i}) to run BEFORE back-edge Op::Load ({load_i})"
        );
        assert!(
            load_i < xor_i,
            "expected unblocked back-edge Op::Load ({load_i}) to hoist BEFORE unrelated low-height Op::Xor ({xor_i}) once `st` is dead"
        );

        for &(nv, stv, mk, xv) in &[(1u64, 15u64, 0xfu64, 9u64), (5, 23, 0x3f, 17)] {
            let args = [
                Val::Int(u128::from(nv)),
                Val::Int(u128::from(stv)),
                Val::Int(u128::from(mk)),
                Val::Int(u128::from(xv)),
            ];
            let Outcome::Returned(r0) =
                eval::run_in(func2.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r1) = eval::run_in(sv2, &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            assert_eq!(r0, r1, "post-kill load hoist mismatch on ({nv}, {stv}, {mk}, {xv})");
        }
    }

    #[test]
    fn schedule_dag_defers_multiblock_loop_exit_select_when_early_exit_reads_param() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // 1. Exact multi-block loop shape of `decode_exec_leaf_single`:
        //    - `loop_hdr` (Block 1) owns param `r2` and has an early exit to `exit_done` reading
        //      `r2` (making `use_block[r2] == MULTI_BLOCK`, so `seg_live_out[r2] == true`).
        //    - `loop_body` (Block 2) computes `next_r2 = select(rc < 2, r2, r1)` (earlier in block)
        //      and `rep_hi = select(rc == 2, r2, r0_m2)` (delayed behind 2 muls) and branches to
        //      `loop_latch` / `exit_bail` (NO back-edge on Block 2's terminator; `exit_bail`
        //      stores `next_r2`).
        //    - `loop_latch` (Block 3) holds the back-edge `jump loop_hdr(..., next_r2, ...)`.
        //    Whole-function loop-param replacement tracking must still defer `next_r2` until
        //    `rep_hi` executes inside `loop_body`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let bail_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_hdr, loop_body, loop_latch, exit_done, exit_bail] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let rc_in = b.param(Ty::I64);
        let r0_in = b.param(Ty::I64);
        let r1_in = b.param(Ty::I64);
        let r2_in = b.param(Ty::I64);
        let out_p = b.slot_addr(out_slot);
        let bail_p = b.slot_addr(bail_slot);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        b.jump((loop_hdr, &[rc_in, r0_in, r1_in, r2_in, c0, n]));

        b.begin(loop_hdr);
        let rc = b.param(Ty::I64);
        let r0 = b.param(Ty::I64);
        let r1 = b.param(Ty::I64);
        let r2 = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);
        let cond0 = b.icmp(IntCond::Ne, rem, c0);
        b.br(Flags::NONE, cond0, (loop_body, &[]), (exit_done, &[]));

        b.begin(loop_body);
        let cmp_lt2 = b.icmp(IntCond::Ult, rc, c2);
        let next_r2 = b.select(Flags::NONE, cmp_lt2, r2, r1);
        let r0_m1 = b.binary(Op::Mul, Flags::NONE, r0, c3);
        let r0_m2 = b.binary(Op::Mul, Flags::NONE, r0_m1, r0);
        let cmp_eq2 = b.icmp(IntCond::Eq, rc, c2);
        let rep_hi = b.select(Flags::NONE, cmp_eq2, r2, r0_m2);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, rep_hi);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cond1 = b.icmp(IntCond::Ne, rep_hi, c0);
        b.br(Flags::NONE, cond1, (loop_latch, &[]), (exit_bail, &[]));

        b.begin(loop_latch);
        b.store(Flags::NONE, next_acc, out_p, 0, MemAttrs::aligned(3));
        b.jump((loop_hdr, &[rc, r0, r1, next_r2, next_acc, next_rem]));

        b.begin(exit_done);
        let done_res = b.binary(Op::Add, Flags::NONE, acc, r2);
        b.ret(&[done_res]);

        b.begin(exit_bail);
        b.store(Flags::NONE, next_r2, bail_p, 0, MemAttrs::aligned(3));
        let saved = b.load(Ty::I64, Flags::NONE, bail_p, 0, MemAttrs::aligned(3));
        let bail_res = b.binary(Op::Add, Flags::NONE, next_acc, saved);
        b.ret(&[bail_res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let body_range = sv.block(Block(2));
        let mut same_blk_uses = filled(sv.len(), 0u32, &TestHeap);
        for inst in body_range.clone() {
            each(&sv, inst, |item| {
                if let Item::Value(v) = item {
                    if let Some(slot) = same_blk_uses.get_mut(to_idx(v)) {
                        *slot = slot.saturating_add(1);
                    }
                }
            });
        }
        let mut rep_hi_sel = None;
        let mut next_r2_sel = None;
        for inst in body_range {
            if sv.op(inst) == Op::Select {
                if at_u32(&same_blk_uses, inst) > 0 {
                    rep_hi_sel = Some(inst);
                } else {
                    next_r2_sel = Some(inst);
                }
            }
        }
        let rep_hi_pos = rep_hi_sel.unwrap();
        let next_r2_pos = next_r2_sel.unwrap();
        assert!(
            rep_hi_pos < next_r2_pos,
            "expected rep_hi Op::Select ({rep_hi_pos}) to run BEFORE multi-block back-edge next_r2 Op::Select ({next_r2_pos})"
        );

        for &(nv, rc_v, r2_v) in &[
            (4u64, 0u64, 47u64),
            (4, 1, 47),
            (4, 2, 47),
            (4, 3, 47),
            (0, 2, 47),
            (4, 2, 0),
        ] {
            let args = [
                Val::Int(u128::from(nv)),
                Val::Int(u128::from(rc_v)),
                Val::Int(11),
                Val::Int(29),
                Val::Int(u128::from(r2_v)),
            ];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "multiblock loop select deferral mismatch on (n={nv}, rc={rc_v}, r2={r2_v})"
            );
        }

        // 2. Multi-block convert chain (`Load -> Zext`) replacing a loop-header param `st` on a
        //    later latch block's back-edge where `raw_next` does not itself read `st`:
        //    `global_rep_head` must still defer `raw_next` until `st_bits = and(st, mask)` runs.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let tbl_slot = b2.slot(8, 1, SlotKind::Temp);
        let out_slot2 = b2.slot(8, 3, SlotKind::Temp);
        let [entry2, hdr2, body2, latch2, done2, bail2] =
            [(); 6].map(|_| b2.create_block(BlockKind::Normal));

        b2.begin(entry2);
        let n2 = b2.param(Ty::I64);
        let init_st = b2.param(Ty::I64);
        let mask2 = b2.param(Ty::I64);
        let tbl_p = b2.slot_addr(tbl_slot);
        let out_p2 = b2.slot_addr(out_slot2);
        let c42 = b2.iconst(Ty::I16, 42);
        b2.store(Flags::NONE, c42, tbl_p, 0, MemAttrs::aligned(1));
        let z0 = b2.iconst(Ty::I64, 0);
        let z1 = b2.iconst(Ty::I64, 1);
        b2.jump((hdr2, &[init_st, z0, n2]));

        b2.begin(hdr2);
        let st = b2.param(Ty::I64);
        let acc2 = b2.param(Ty::I64);
        let rem2 = b2.param(Ty::I64);
        let cond0_2 = b2.icmp(IntCond::Ne, rem2, z0);
        b2.br(Flags::NONE, cond0_2, (body2, &[]), (done2, &[]));

        b2.begin(body2);
        let raw_next = b2.load(Ty::I16, Flags::NONE, tbl_p, 0, MemAttrs::aligned(1));
        let next_st = b2.convert(Op::Zext, Flags::NONE, Ty::I64, raw_next);
        let st_bits = b2.binary(Op::And, Flags::NONE, st, mask2);
        let next_acc2 = b2.binary(Op::Add, Flags::NONE, acc2, st_bits);
        let next_rem2 = b2.binary(Op::Sub, Flags::NONE, rem2, z1);
        let cond1_2 = b2.icmp(IntCond::Ne, next_acc2, z0);
        b2.br(Flags::NONE, cond1_2, (latch2, &[]), (bail2, &[]));

        b2.begin(latch2);
        b2.store(Flags::NONE, next_acc2, out_p2, 0, MemAttrs::aligned(3));
        b2.jump((hdr2, &[next_st, next_acc2, next_rem2]));

        b2.begin(done2);
        let done_res2 = b2.binary(Op::Add, Flags::NONE, acc2, st);
        b2.ret(&[done_res2]);

        b2.begin(bail2);
        let bail_res2 = b2.binary(Op::Add, Flags::NONE, next_acc2, next_st);
        b2.ret(&[bail_res2]);
        let func2 = b2.finish_checked().unwrap();

        let sched2 = schedule_dag(func2.view(), TestHeap).unwrap();
        let sv2 = sched2.func.view();
        verify(sv2).unwrap();

        let mut and_pos = None;
        let mut load_pos = None;
        for inst in sv2.block(Block(2)) {
            match sv2.op(inst) {
                Op::And => and_pos = Some(inst),
                Op::Load => load_pos = Some(inst),
                _ => {}
            }
        }
        let and_i = and_pos.unwrap();
        let load_i = load_pos.unwrap();
        assert!(
            and_i < load_i,
            "expected dying-state Op::And ({and_i}) to run BEFORE multi-block back-edge Load->Zext ({load_i})"
        );

        for &(nv, stv, mk) in &[(3u64, 15u64, 0xfu64), (0, 15, 0xf), (3, 0, 0xf)] {
            let args = [
                Val::Int(u128::from(nv)),
                Val::Int(u128::from(stv)),
                Val::Int(u128::from(mk)),
            ];
            let Outcome::Returned(r0) =
                eval::run_in(func2.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r1) = eval::run_in(sv2, &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            assert_eq!(
                r0, r1,
                "multiblock load->zext deferral mismatch on (n={nv}, st={stv}, mk={mk})"
            );
        }
    }

    #[test]
    fn schedule_dag_tracks_transitive_latch_param_loop_replacement() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `decode_exec_leaf_single`:
        // - `hdr(r1, r2, r3, rem)` has an early exit reading `r2` (so `r2` is `MULTI_BLOCK` /
        //   `seg_live_out == true`).
        // - `body` computes `next_r2 = select(lt2, r2, r3)` (ready early), `rep_hi = select(lt2,
        //   r3, r1 - 1)` (behind latency so `next_r2` would hoist without transitive param
        //   tracking), `r2_or_hi = select(eq2, r2, rep_hi)`, `rep_off = select(eq0, r1, r2_or_hi)`
        //   (used only in `arm_a` and `arm_b`, so `only_feeds_exit == true` for both `r2_or_hi`
        //   and `next_r2` and `rem_non_exit_uses[r2] == 0`), and branches to `arm_a` / `arm_b`.
        // - Both `arm_a` and `arm_b` jump to `latch(l_r1, l_r2, l_r3, l_rem)` passing `next_r2`
        //   for `l_r2`, and `latch` decrements `l_rem` and takes the back-edge to
        //   `hdr(l_r1, l_r2, l_r3, next_rem)` or exits.
        // Transitive `Op::Param` tracing in `collect_global_loop_reps` must link `next_r2 -> r2`
        // through `l_r2` and defer `next_r2` strictly after `r2_or_hi`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, hdr, body, arm_a, arm_b, latch, exit_early, exit_done] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let rc_in = b.param(Ty::I64);
        let r1_in = b.param(Ty::I64);
        let r2_in = b.param(Ty::I64);
        let r3_in = b.param(Ty::I64);
        let out_p = b.slot_addr(out_slot);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        b.store(Flags::NONE, c0, out_p, 0, MemAttrs::aligned(3));
        b.jump((hdr, &[r1_in, r2_in, r3_in, n]));

        b.begin(hdr);
        let r1 = b.param(Ty::I64);
        let r2 = b.param(Ty::I64);
        let r3 = b.param(Ty::I64);
        let rem = b.param(Ty::I64);
        let has_rem = b.icmp(IntCond::Ne, rem, c0);
        b.br(Flags::NONE, has_rem, (body, &[]), (exit_early, &[]));

        b.begin(body);
        let lt2 = b.icmp(IntCond::Ult, rc_in, c2);
        let next_r2 = b.select(Flags::NONE, lt2, r2, r3);
        let r1_d1 = b.binary(Op::Mul, Flags::NONE, r1, c1);
        let r1_d2 = b.binary(Op::Mul, Flags::NONE, r1_d1, c1);
        let r1_d3 = b.binary(Op::Mul, Flags::NONE, r1_d2, c1);
        let r1_m1 = b.binary(Op::Sub, Flags::NONE, r1_d3, c1);
        let rep_hi = b.select(Flags::NONE, lt2, r3, r1_m1);
        let eq2 = b.icmp(IntCond::Eq, rc_in, c2);
        let r2_or_hi = b.select(Flags::NONE, eq2, r2, rep_hi);
        let eq0 = b.icmp(IntCond::Eq, rc_in, c0);
        let rep_off = b.select(Flags::NONE, eq0, r1, r2_or_hi);
        let take_a = b.icmp(IntCond::Ult, rem, c3);
        b.br(Flags::NONE, take_a, (arm_a, &[]), (arm_b, &[]));

        b.begin(arm_a);
        let prev_a = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let acc_a = b.binary(Op::Add, Flags::NONE, prev_a, rep_off);
        b.store(Flags::NONE, acc_a, out_p, 0, MemAttrs::aligned(3));
        let arm_a_r1 = b.binary(Op::Add, Flags::NONE, rep_off, c1);
        b.jump((latch, &[arm_a_r1, next_r2, r1, rem]));

        b.begin(arm_b);
        let prev_b = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let acc_b = b.binary(Op::Xor, Flags::NONE, prev_b, rep_off);
        b.store(Flags::NONE, acc_b, out_p, 0, MemAttrs::aligned(3));
        b.jump((latch, &[rep_off, next_r2, r3, rem]));

        b.begin(latch);
        let l_r1 = b.param(Ty::I64);
        let l_r2 = b.param(Ty::I64);
        let l_r3 = b.param(Ty::I64);
        let l_rem = b.param(Ty::I64);
        let next_rem = b.binary(Op::Sub, Flags::NONE, l_rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.br(
            Flags::NONE,
            cont,
            (hdr, &[l_r1, l_r2, l_r3, next_rem]),
            (exit_done, &[l_r1, l_r2]),
        );

        b.begin(exit_early);
        let early_acc = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let early_res = b.binary(Op::Add, Flags::NONE, early_acc, r2);
        b.ret(&[early_res]);

        b.begin(exit_done);
        let done_r1 = b.param(Ty::I64);
        let done_r2 = b.param(Ty::I64);
        let done_acc = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let done_s1 = b.binary(Op::Add, Flags::NONE, done_acc, done_r1);
        let done_res = b.binary(Op::Add, Flags::NONE, done_s1, done_r2);
        b.ret(&[done_res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let hdr_params = sv.params(Block(1));
        let r2_vi = hdr_params.start + 1;
        let body_range = sv.block(Block(2));
        let mut same_blk_uses = filled(sv.len(), 0u32, &TestHeap);
        for inst in body_range.clone() {
            each(&sv, inst, |item| {
                if let Item::Value(v) = item {
                    if let Some(slot) = same_blk_uses.get_mut(to_idx(v)) {
                        *slot = slot.saturating_add(1);
                    }
                }
            });
        }
        let mut r2_or_hi_sel = None;
        let mut next_r2_sel = None;
        for inst in body_range {
            if sv.op(inst) == Op::Select {
                let mut reads_r2 = false;
                each(&sv, inst, |item| {
                    if let Item::Value(v) = item {
                        if to_idx(v) == r2_vi {
                            reads_r2 = true;
                        }
                    }
                });
                if reads_r2 {
                    if at_u32(&same_blk_uses, inst) > 0 {
                        r2_or_hi_sel = Some(inst);
                    } else {
                        next_r2_sel = Some(inst);
                    }
                }
            }
        }
        let r2_or_hi_pos = r2_or_hi_sel.unwrap();
        let next_r2_pos = next_r2_sel.unwrap();
        assert!(
            r2_or_hi_pos < next_r2_pos,
            "expected r2_or_hi Op::Select ({r2_or_hi_pos}) to run BEFORE transitive latch-param next_r2 Op::Select ({next_r2_pos})"
        );

        for &(nv, rc_v) in &[
            (0u64, 2u64),
            (1, 0),
            (4, 0),
            (4, 1),
            (4, 2),
            (4, 3),
        ] {
            let args = [
                Val::Int(u128::from(nv)),
                Val::Int(u128::from(rc_v)),
                Val::Int(11),
                Val::Int(29),
                Val::Int(47),
            ];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "transitive latch-param select deferral mismatch on (n={nv}, rc={rc_v})"
            );
        }
    }

    #[test]
    fn pressure_budget_accounts_for_live_through_loop_invariants() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `decode_exec_leaf_single`: a loop block with 16 live-through `Op::Param`s
        // (13 unread live-through + 3 read in segment) + 4 independent chains where each chain
        // opens 3 live temporaries before reducing them. Ignoring live-through pressure would
        // open all 12 leaves simultaneously (`16 + 12 = 28 > 27` GPRs -> stack spill). With
        // live-through liveness factored into `seg_pressure_high`, `schedule_dag` still overlaps
        // chains for ILP (`peak_intra >= 4`) while draining early chains before opening all 4
        // simultaneously (`peak_intra <= 8`, total GPRs <= 24).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let out_p = b.slot_addr(out_slot);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let mut ks: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(12, TestHeap);
        for i in 0u128..12 {
            ks.push(b.iconst(Ty::I64, 3 + i * 2));
        }
        let mut init_args: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(16, TestHeap);
        for i in 0u128..15 {
            let off = b.iconst(Ty::I64, i + 1);
            init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
        }
        init_args.push(n);
        let init_slice: &[crate::Value] = &init_args;
        b.jump((loop_blk, init_slice));

        b.begin(loop_blk);
        let live_params: [crate::Value; 13] = core::array::from_fn(|_| b.param(Ty::I64));
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let rem = b.param(Ty::I64);

        let mut chain_outs: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(4, TestHeap);
        for j in 0..4 {
            let ka = ks.get(j * 3).copied().unwrap();
            let kb = ks.get(j * 3 + 1).copied().unwrap();
            let kc = ks.get(j * 3 + 2).copied().unwrap();
            let a = b.binary(Op::Mul, Flags::NONE, x, ka);
            let bv = b.binary(Op::Mul, Flags::NONE, y, kb);
            let c = b.binary(Op::Mul, Flags::NONE, x, kc);
            let ab = b.binary(Op::Add, Flags::NONE, a, bv);
            let w = b.binary(Op::Xor, Flags::NONE, ab, c);
            chain_outs.push(w);
        }
        let w0 = chain_outs.get(0).copied().unwrap();
        let w1 = chain_outs.get(1).copied().unwrap();
        let w2 = chain_outs.get(2).copied().unwrap();
        let w3 = chain_outs.get(3).copied().unwrap();
        let s01 = b.binary(Op::Xor, Flags::NONE, w0, w1);
        let s23 = b.binary(Op::Xor, Flags::NONE, w2, w3);
        let sum = b.binary(Op::Add, Flags::NONE, s01, s23);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.store(Flags::NONE, sum, out_p, 0, MemAttrs::aligned(3));
        let mut back_args: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(16, TestHeap);
        for &p in &live_params {
            back_args.push(p);
        }
        back_args.push(x);
        back_args.push(y);
        back_args.push(next_rem);
        let back_slice: &[crate::Value] = &back_args;
        b.br(Flags::NONE, cont, (loop_blk, back_slice), (exit_blk, &[]));

        b.begin(exit_blk);
        let saved = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let mut acc = b.binary(Op::Add, Flags::NONE, saved, x);
        acc = b.binary(Op::Add, Flags::NONE, acc, y);
        for &p in &live_params {
            acc = b.binary(Op::Add, Flags::NONE, acc, p);
        }
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        // Measure peak live intra-segment values inside `loop_blk` (`Block(1)`).
        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        let term = loop_range.end - 1;
        let mut rem_uses = filled(sv.len(), 0u32, &TestHeap);
        let mut inst_ops: Vec<Vec<usize, TestHeap>, TestHeap> =
            Vec::with_capacity_in(sv.len(), TestHeap);
        for inst in 0..sv.len() {
            let mut ops: Vec<usize, TestHeap> = Vec::new_in(TestHeap);
            each(&sv, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if ops.scan_for(&vi).is_none() {
                        ops.push(vi);
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            *slot += 1;
                        }
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = to_idx(at_u32(sv.extra, start + k));
                            if ops.scan_for(&vi).is_none() {
                                ops.push(vi);
                                if let Some(slot) = rem_uses.get_mut(vi) {
                                    *slot += 1;
                                }
                            }
                        }
                    }
                }
            });
            inst_ops.push(ops);
        }

        let mut intra_live = 0usize;
        let mut peak_intra = 0usize;
        for inst in seg_start..term {
            let ty = Ty(at_u16(sv.tys, inst));
            if counts_for_live_pressure(ty, sv.op(inst)) && at_u32(&rem_uses, inst) > 0 {
                intra_live += 1;
            }
            if let Some(ops) = inst_ops.get(inst) {
                for &vi in ops {
                    if vi >= seg_start
                        && vi < term
                        && counts_for_live_pressure(Ty(at_u16(sv.tys, vi)), sv.op(vi))
                    {
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            if *slot > 0 {
                                *slot -= 1;
                                if *slot == 0 {
                                    intra_live = intra_live.saturating_sub(1);
                                }
                            }
                        }
                    }
                }
            }
            if intra_live > peak_intra {
                peak_intra = intra_live;
            }
        }
        assert!(
            peak_intra <= 8,
            "expected live-through-aware schedule_dag to bound peak intra-segment live values <= 8 (naive peaks at 12), got {peak_intra}"
        );
        assert!(
            peak_intra >= 4,
            "expected schedule_dag to still overlap independent chains before hitting budget, got peak_intra={peak_intra}"
        );

        for &(nv, sv_in) in &[(1u64, 7u64), (3, 19), (5, 41)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(sv_in))];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "live-through pressure schedule mismatch on (n={nv}, seed={sv_in})"
            );
        }
    }

    #[test]
    fn schedule_dag_defers_exit_only_packed_word_extractions_until_non_exit_consumer_runs() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `decode_exec_leaf_single`: a packed-word load `e_of` feeds BOTH a multi-step
        // non-exit compute chain (`ll0 -> ll1 -> ll2 -> of_val -> Store`) and multi-op
        // `only_feeds_exit` extraction chains (`Lshr(56) -> Not`, `Lshr(32) -> And(0xffff) -> Add`).
        // The `Load` must still hoist to the top of the loop segment, while the exit-only
        // intermediate ALU extractions must wait until `of_val` runs so `e_of` and its extracted
        // children are not kept live simultaneously (peak intra-segment live <= 5 vs 6 if hoisted).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let tbl_slot = b.slot(8, 3, SlotKind::Temp);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let bits_in = b.param(Ty::I64);
        let tbl_p = b.slot_addr(tbl_slot);
        let out_p = b.slot_addr(out_slot);
        b.store(Flags::NONE, seed, tbl_p, 0, MemAttrs::aligned(3));
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c32 = b.iconst(Ty::I64, 32);
        let c56 = b.iconst(Ty::I64, 56);
        let mask16 = b.iconst(Ty::I64, 0xffff);
        b.jump((loop_blk, &[bits_in, c0, c0, c0, n]));

        b.begin(loop_blk);
        let bits = b.param(Ty::I64);
        let of_state = b.param(Ty::I64);
        let of_nb = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);

        // Multi-step non-exit `ll` compute chain placed BEFORE `e_of`:
        let ll0 = b.binary(Op::Mul, Flags::NONE, bits, c3);
        let ll1 = b.binary(Op::Xor, Flags::NONE, ll0, of_nb);
        let ll2 = b.binary(Op::Add, Flags::NONE, ll1, acc);
        // Packed-word load (`e_of`) feeding both `of_val` (non-exit) and exit-only extractions:
        let e_of = b.load(Ty::I64, Flags::NONE, tbl_p, 0, MemAttrs::aligned(3));
        // Exit-only intermediate extraction chains placed BEFORE `of_val`:
        let sh56 = b.binary(Op::Lshr, Flags::NONE, e_of, c56);
        let not56 = b.unary(Op::Not, Flags::NONE, sh56);
        let sh32 = b.binary(Op::Lshr, Flags::NONE, e_of, c32);
        let mask32 = b.binary(Op::And, Flags::NONE, sh32, mask16);
        let next_of = b.binary(Op::Add, Flags::NONE, mask32, of_state);
        // Non-exit consumer of `e_of` feeding `Store`:
        let of_val = b.binary(Op::Add, Flags::NONE, ll2, e_of);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.store(Flags::NONE, of_val, out_p, 0, MemAttrs::aligned(3));
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, &[bits, next_of, not56, of_val, next_rem]),
            (exit_blk, &[next_of, not56, of_val]),
        );

        b.begin(exit_blk);
        let out_of = b.param(Ty::I64);
        let out_nb = b.param(Ty::I64);
        let out_val = b.param(Ty::I64);
        let s0 = b.binary(Op::Xor, Flags::NONE, out_of, out_nb);
        let s1 = b.binary(Op::Add, Flags::NONE, s0, out_val);
        let saved = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let res = b.binary(Op::Add, Flags::NONE, s1, saved);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        let term = loop_range.end - 1;

        // 1. Packed-word `Load` (`e_of`) still hoists to the very top of the loop segment.
        assert_eq!(
            sv.op(seg_start),
            Op::Load,
            "expected packed-word Load (`e_of`) to still schedule at the top of the loop segment"
        );

        // 2. Exit-only intermediate extractions (`Lshr`, `And`, `Not`) must NOT hoist ahead of
        //    the non-exit consumer `of_val = Add(ll2, e_of)`.
        let mut of_val_pos = None;
        let mut first_lshr_pos = None;
        let mut lshr_count = 0usize;
        let mut and_pos = None;
        let mut not_pos = None;
        for inst in seg_start..term {
            match sv.op(inst) {
                Op::Add => {
                    let mut reads_e_of = false;
                    each(&sv, inst, |item| {
                        if let Item::Value(v) = item {
                            if to_idx(v) == seg_start {
                                reads_e_of = true;
                            }
                        }
                    });
                    if reads_e_of {
                        of_val_pos = Some(inst);
                    }
                }
                Op::Lshr => {
                    lshr_count += 1;
                    if first_lshr_pos.is_none() {
                        first_lshr_pos = Some(inst);
                    }
                }
                Op::And => and_pos = Some(inst),
                Op::Not => not_pos = Some(inst),
                _ => {}
            }
        }
        let of_val_i = of_val_pos.unwrap();
        let first_lshr_i = first_lshr_pos.unwrap();
        let and_i = and_pos.unwrap();
        let not_i = not_pos.unwrap();
        assert_eq!(lshr_count, 2);
        assert!(
            of_val_i < first_lshr_i,
            "expected non-exit `of_val` ({of_val_i}) to run BEFORE exit-only Op::Lshr ({first_lshr_i})"
        );
        assert!(
            of_val_i < and_i,
            "expected non-exit `of_val` ({of_val_i}) to run BEFORE exit-only Op::And ({and_i})"
        );
        assert!(
            of_val_i < not_i,
            "expected non-exit `of_val` ({of_val_i}) to run BEFORE exit-only Op::Not ({not_i})"
        );

        // 3. Peak live intra-segment values stay strictly lower than early hoisting (<= 5 vs 6).
        let mut rem_uses = filled(sv.len(), 0u32, &TestHeap);
        let mut inst_ops: Vec<Vec<usize, TestHeap>, TestHeap> =
            Vec::with_capacity_in(sv.len(), TestHeap);
        for inst in 0..sv.len() {
            let mut ops: Vec<usize, TestHeap> = Vec::new_in(TestHeap);
            each(&sv, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if ops.scan_for(&vi).is_none() {
                        ops.push(vi);
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            *slot += 1;
                        }
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = to_idx(at_u32(sv.extra, start + k));
                            if ops.scan_for(&vi).is_none() {
                                ops.push(vi);
                                if let Some(slot) = rem_uses.get_mut(vi) {
                                    *slot += 1;
                                }
                            }
                        }
                    }
                }
            });
            inst_ops.push(ops);
        }
        let mut intra_live = 0usize;
        let mut peak_intra = 0usize;
        for inst in seg_start..term {
            let ty = Ty(at_u16(sv.tys, inst));
            if counts_for_live_pressure(ty, sv.op(inst)) && at_u32(&rem_uses, inst) > 0 {
                intra_live += 1;
            }
            if let Some(ops) = inst_ops.get(inst) {
                for &vi in ops {
                    if vi >= seg_start
                        && vi < term
                        && counts_for_live_pressure(Ty(at_u16(sv.tys, vi)), sv.op(vi))
                    {
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            if *slot > 0 {
                                *slot -= 1;
                                if *slot == 0 {
                                    intra_live = intra_live.saturating_sub(1);
                                }
                            }
                        }
                    }
                }
            }
            if intra_live > peak_intra {
                peak_intra = intra_live;
            }
        }
        assert!(
            peak_intra <= 5,
            "expected deferred exit-only extractions to keep peak intra-segment live <= 5 (early hoisting peaks at 6), got {peak_intra}"
        );

        // 4. Scheduled function evaluates identically to unscheduled input.
        for &(nv, seed_v, bits_v) in &[
            (1u64, 0xab_cd_1234_5678_9ef0u64, 0x1111_2222u64),
            (4, 0x05_00_7fff_0001_0042, 0xdead_beef),
            (7, 0xff_12_abcd_0000_0007, 0x0123_4567_89ab_cdef),
        ] {
            let args = [
                Val::Int(u128::from(nv)),
                Val::Int(u128::from(seed_v)),
                Val::Int(u128::from(bits_v)),
            ];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "packed-word exit extraction schedule mismatch on (n={nv}, seed={seed_v:#x}, bits={bits_v:#x})"
            );
        }
    }

    #[test]
    fn schedule_dag_defers_exit_only_alu_under_high_live_through_after_early_trunc() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `decode_exec_leaf_single`: with `seg_live_through == 12` (total live pressure
        // >= `EXIT_ALU_DEFER_PRESSURE`), `e_of_lo = Trunc(e_of, I32)` schedules right after
        // `e_of = Load(...)`, dropping `rem_non_exit_uses[e_of]` to `0` while `e_of_lo` stays
        // live until the late non-exit `of_val = Add(ll3_32, e_of_lo)`. Even once
        // `blocks_operand_kill` clears for `e_of`, exit-only ALU ops (`e_of >> 56 -> Not` and
        // `e_of >> 32 -> And(0xffff) -> Add`) must defer behind the non-exit `Add` so peak live
        // stays bounded without pinning extra exit-only registers across the block body.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let tbl_slot = b.slot(8, 3, SlotKind::Temp);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let bits_in = b.param(Ty::I64);
        let tbl_p = b.slot_addr(tbl_slot);
        let out_p = b.slot_addr(out_slot);
        b.store(Flags::NONE, seed, tbl_p, 0, MemAttrs::aligned(3));
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c32 = b.iconst(Ty::I64, 32);
        let c56 = b.iconst(Ty::I64, 56);
        let mask16 = b.iconst(Ty::I64, 0xffff);
        let mut init_args: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(17, TestHeap);
        for i in 0u128..12 {
            let off = b.iconst(Ty::I64, i + 1);
            init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
        }
        init_args.push(bits_in);
        init_args.push(c0);
        init_args.push(c0);
        init_args.push(c0);
        init_args.push(n);
        let init_slice: &[crate::Value] = &init_args;
        b.jump((loop_blk, init_slice));

        b.begin(loop_blk);
        let live_params: [crate::Value; 12] = core::array::from_fn(|_| b.param(Ty::I64));
        let bits = b.param(Ty::I64);
        let of_state = b.param(Ty::I64);
        let of_nb = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);

        // Delayed non-exit compute chain culminating in `of_val = Add(ll3_32, e_of_lo)`:
        let ll0 = b.binary(Op::Mul, Flags::NONE, bits, c3);
        let ll1 = b.binary(Op::Mul, Flags::NONE, ll0, bits);
        let ll2 = b.binary(Op::Mul, Flags::NONE, ll1, of_nb);
        let ll3 = b.binary(Op::Add, Flags::NONE, ll2, acc);
        let ll3_32 = b.convert(Op::Trunc, Flags::NONE, Ty::I32, ll3);
        // Packed-word load `e_of` + early `Trunc(e_of, I32)` feeding the late non-exit `Add`:
        let e_of = b.load(Ty::I64, Flags::NONE, tbl_p, 0, MemAttrs::aligned(3));
        let e_of_lo = b.convert(Op::Trunc, Flags::NONE, Ty::I32, e_of);
        // Exit-only ALU extraction chains (`only_feeds_exit == true`, `!is_load`):
        let sh56 = b.binary(Op::Lshr, Flags::NONE, e_of, c56);
        let not56 = b.unary(Op::Not, Flags::NONE, sh56);
        let sh32 = b.binary(Op::Lshr, Flags::NONE, e_of, c32);
        let mask32 = b.binary(Op::And, Flags::NONE, sh32, mask16);
        let next_of = b.binary(Op::Add, Flags::NONE, mask32, of_state);
        // Late non-exit `Add` consuming `e_of_lo` -> `Store`:
        let of_val = b.binary(Op::Add, Flags::NONE, ll3_32, e_of_lo);
        let of_val_64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, of_val);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.store(Flags::NONE, of_val_64, out_p, 0, MemAttrs::aligned(3));
        let mut back_args: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(17, TestHeap);
        for &p in &live_params {
            back_args.push(p);
        }
        back_args.push(bits);
        back_args.push(next_of);
        back_args.push(not56);
        back_args.push(of_val_64);
        back_args.push(next_rem);
        let back_slice: &[crate::Value] = &back_args;
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, back_slice),
            (exit_blk, &[next_of, not56, of_val_64]),
        );

        b.begin(exit_blk);
        let out_of = b.param(Ty::I64);
        let out_nb = b.param(Ty::I64);
        let out_val = b.param(Ty::I64);
        let saved = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let mut res = b.binary(Op::Xor, Flags::NONE, out_of, out_nb);
        res = b.binary(Op::Add, Flags::NONE, res, out_val);
        res = b.binary(Op::Add, Flags::NONE, res, saved);
        for &p in &live_params {
            res = b.binary(Op::Add, Flags::NONE, res, p);
        }
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        let term = loop_range.end - 1;

        let mut load_pos = None;
        let mut of_val_pos = None;
        let mut first_lshr_pos = None;
        let mut lshr_count = 0usize;
        let mut and_pos = None;
        let mut not_pos = None;
        let mut next_of_pos = None;
        for inst in seg_start..term {
            match sv.op(inst) {
                Op::Load => load_pos = Some(inst),
                Op::Add => {
                    if Ty(at_u16(sv.tys, inst)) == Ty::I32 {
                        of_val_pos = Some(inst);
                    } else if let Some(and_i) = and_pos {
                        let mut reads_and = false;
                        each(&sv, inst, |item| {
                            if let Item::Value(v) = item {
                                if to_idx(v) == and_i {
                                    reads_and = true;
                                }
                            }
                        });
                        if reads_and {
                            next_of_pos = Some(inst);
                        }
                    }
                }
                Op::Lshr => {
                    lshr_count += 1;
                    if first_lshr_pos.is_none() {
                        first_lshr_pos = Some(inst);
                    }
                }
                Op::And => and_pos = Some(inst),
                Op::Not => not_pos = Some(inst),
                _ => {}
            }
        }
        let load_i = load_pos.unwrap();
        let of_val_i = of_val_pos.unwrap();
        let first_lshr_i = first_lshr_pos.unwrap();
        let and_i = and_pos.unwrap();
        let not_i = not_pos.unwrap();
        let next_of_i = next_of_pos.unwrap();
        assert_eq!(lshr_count, 2);
        assert!(
            load_i < of_val_i,
            "expected packed-word Load ({load_i}) to still schedule early before non-exit Add ({of_val_i})"
        );
        assert!(
            of_val_i < first_lshr_i,
            "expected non-exit Add ({of_val_i}) to run BEFORE deferred exit-only Op::Lshr ({first_lshr_i})"
        );
        assert!(
            of_val_i < and_i,
            "expected non-exit Add ({of_val_i}) to run BEFORE deferred exit-only Op::And ({and_i})"
        );
        assert!(
            of_val_i < not_i,
            "expected non-exit Add ({of_val_i}) to run BEFORE deferred exit-only Op::Not ({not_i})"
        );
        assert!(
            of_val_i < next_of_i,
            "expected non-exit Add ({of_val_i}) to run BEFORE deferred exit-only next_of Op::Add ({next_of_i})"
        );

        let mut rem_uses = filled(sv.len(), 0u32, &TestHeap);
        let mut inst_ops: Vec<Vec<usize, TestHeap>, TestHeap> =
            Vec::with_capacity_in(sv.len(), TestHeap);
        for inst in 0..sv.len() {
            let mut ops: Vec<usize, TestHeap> = Vec::new_in(TestHeap);
            each(&sv, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if ops.scan_for(&vi).is_none() {
                        ops.push(vi);
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            *slot += 1;
                        }
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = to_idx(at_u32(sv.extra, start + k));
                            if ops.scan_for(&vi).is_none() {
                                ops.push(vi);
                                if let Some(slot) = rem_uses.get_mut(vi) {
                                    *slot += 1;
                                }
                            }
                        }
                    }
                }
            });
            inst_ops.push(ops);
        }
        let mut intra_live = 0usize;
        let mut peak_intra = 0usize;
        for inst in seg_start..term {
            let ty = Ty(at_u16(sv.tys, inst));
            if counts_for_live_pressure(ty, sv.op(inst)) && at_u32(&rem_uses, inst) > 0 {
                intra_live += 1;
            }
            if let Some(ops) = inst_ops.get(inst) {
                for &vi in ops {
                    if vi >= seg_start
                        && vi < term
                        && counts_for_live_pressure(Ty(at_u16(sv.tys, vi)), sv.op(vi))
                    {
                        if let Some(slot) = rem_uses.get_mut(vi) {
                            if *slot > 0 {
                                *slot -= 1;
                                if *slot == 0 {
                                    intra_live = intra_live.saturating_sub(1);
                                }
                            }
                        }
                    }
                }
            }
            if intra_live > peak_intra {
                peak_intra = intra_live;
            }
        }
        assert!(
            peak_intra <= 5,
            "expected deferred exit-only ALU under high live-through pressure to bound peak intra-segment live <= 5, got {peak_intra}"
        );

        for &(nv, seed_v, bits_v) in &[
            (1u64, 0xab_cd_1234_5678_9ef0u64, 0x1111_2222u64),
            (4, 0x05_00_7fff_0001_0042, 0xdead_beef),
            (7, 0xff_12_abcd_0000_0007, 0x0123_4567_89ab_cdef),
        ] {
            let args = [
                Val::Int(u128::from(nv)),
                Val::Int(u128::from(seed_v)),
                Val::Int(u128::from(bits_v)),
            ];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "high-live-through early-trunc exit-ALU deferral mismatch on (n={nv}, seed={seed_v:#x}, bits={bits_v:#x})"
            );
        }
    }

    #[test]
    fn schedule_dag_does_not_hoist_iv_ptr_add_above_loop_loads() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `xxh64`'s 32-byte stripe loop: `ptr_next = ptr_add(ptr, 32)` feeds BOTH the
        // loop back-edge (`ptr`) and the terminator's `icmp(ptr_next, end) -> br`. All 4 loads
        // (`ptr + 0/8/16/24`) must schedule back-to-back at the top of the loop segment for
        // `ldp` pairing / 4-way ILP, and `ptr_next` must schedule strictly AFTER all 4 loads so
        // `ptr` dies at `ptr_next` and coalesces into a single register with no edge trampoline.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let buf_slot = b.slot(64, 3, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let seed = b.param(Ty::I64);
        let extra = b.param(Ty::I64);
        let buf = b.slot_addr(buf_slot);
        for k in 0u128..8 {
            let off_val = b.iconst(Ty::I64, (k + 1) * 0x0102_0304_0506_0708);
            let elem = b.binary(Op::Add, Flags::NONE, seed, off_val);
            let off = i32::try_from(k * 8).unwrap();
            b.store(Flags::NONE, elem, buf, off, MemAttrs::aligned(3));
        }
        let c32 = b.iconst(Ty::I64, 32);
        let c64 = b.iconst(Ty::I64, 64);
        let end = b.ptr_add(Flags::NONE, buf, c64);
        let p1 = b.iconst(Ty::I64, 0x9E37_79B1_85EB_CA87);
        let p2 = b.iconst(Ty::I64, 0xC2B2_AE3D_27D4_EB4F);
        let r31 = b.iconst(Ty::I64, 31);
        let v1_0 = b.binary(Op::Add, Flags::NONE, seed, p1);
        let v2_0 = b.binary(Op::Add, Flags::NONE, extra, p2);
        let v3_0 = b.binary(Op::Xor, Flags::NONE, seed, extra);
        let v4_0 = b.binary(Op::Sub, Flags::NONE, seed, p1);
        b.jump((loop_blk, &[buf, v1_0, v2_0, v3_0, v4_0]));

        b.begin(loop_blk);
        let ptr = b.param(Ty::PTR);
        let v1 = b.param(Ty::I64);
        let v2 = b.param(Ty::I64);
        let v3 = b.param(Ty::I64);
        let v4 = b.param(Ty::I64);

        // Place `ptr_next` ahead of the 4 loads as well as feeding `Icmp -> Br` and the back-edge.
        let ptr_next = b.ptr_add(Flags::NONE, ptr, c32);
        let k0 = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(3));
        let k1 = b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::aligned(3));
        let k2 = b.load(Ty::I64, Flags::NONE, ptr, 16, MemAttrs::aligned(3));
        let k3 = b.load(Ty::I64, Flags::NONE, ptr, 24, MemAttrs::aligned(3));

        let m0 = b.binary(Op::Mul, Flags::NONE, k0, p2);
        let a0 = b.binary(Op::Add, Flags::NONE, v1, m0);
        let t0 = b.binary(Op::Rotl, Flags::NONE, a0, r31);
        let nv1 = b.binary(Op::Mul, Flags::NONE, t0, p1);

        let m1 = b.binary(Op::Mul, Flags::NONE, k1, p2);
        let a1 = b.binary(Op::Add, Flags::NONE, v2, m1);
        let t1 = b.binary(Op::Rotl, Flags::NONE, a1, r31);
        let nv2 = b.binary(Op::Mul, Flags::NONE, t1, p1);

        let m2 = b.binary(Op::Mul, Flags::NONE, k2, p2);
        let a2 = b.binary(Op::Add, Flags::NONE, v3, m2);
        let t2 = b.binary(Op::Rotl, Flags::NONE, a2, r31);
        let nv3 = b.binary(Op::Mul, Flags::NONE, t2, p1);

        let m3 = b.binary(Op::Mul, Flags::NONE, k3, p2);
        let a3 = b.binary(Op::Add, Flags::NONE, v4, m3);
        let t3 = b.binary(Op::Rotl, Flags::NONE, a3, r31);
        let nv4 = b.binary(Op::Mul, Flags::NONE, t3, p1);

        let cont = b.icmp(IntCond::Ult, ptr_next, end);
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, &[ptr_next, nv1, nv2, nv3, nv4]),
            (exit_blk, &[nv1, nv2, nv3, nv4]),
        );

        b.begin(exit_blk);
        let rv1 = b.param(Ty::I64);
        let rv2 = b.param(Ty::I64);
        let rv3 = b.param(Ty::I64);
        let rv4 = b.param(Ty::I64);
        let s01 = b.binary(Op::Add, Flags::NONE, rv1, rv2);
        let s23 = b.binary(Op::Add, Flags::NONE, rv3, rv4);
        let res = b.binary(Op::Xor, Flags::NONE, s01, s23);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        let mut load_count = 0usize;
        let mut last_load_pos = None;
        let mut ptr_add_pos = None;
        for inst in loop_range {
            match sv.op(inst) {
                Op::Load => {
                    load_count += 1;
                    last_load_pos = Some(inst);
                }
                Op::PtrAdd => ptr_add_pos = Some(inst),
                _ => {}
            }
        }
        assert_eq!(load_count, 4);
        assert_eq!(
            [
                sv.op(seg_start),
                sv.op(seg_start + 1),
                sv.op(seg_start + 2),
                sv.op(seg_start + 3),
            ],
            [Op::Load, Op::Load, Op::Load, Op::Load],
            "expected all 4 loop loads to schedule back-to-back at the top of the loop segment"
        );
        let last_load = last_load_pos.unwrap();
        let ptr_add = ptr_add_pos.unwrap();
        assert!(
            last_load < ptr_add,
            "expected all 4 Op::Load ops (last at {last_load}) to schedule strictly BEFORE IV Op::PtrAdd ({ptr_add})"
        );

        for &(sv_in, ex_in) in &[(0u64, 0u64), (7, 13), (0xdead_beef, 0xcafe_babe)] {
            let args = [Val::Int(u128::from(sv_in)), Val::Int(u128::from(ex_in))];
            let Outcome::Returned(r0) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r1) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap() else {
                panic!()
            };
            assert_eq!(r0, r1, "xxh64 IV ptr_add schedule mismatch on ({sv_in:#x}, {ex_in:#x})");
        }
    }

    #[test]
    fn schedule_dag_hoists_ready_load_ahead_of_zero_delta_alu_under_moderate_pressure() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `lz77_scan_default_leaf` (`inner_scan` Lane 1): under `high_pressure` with
        // `seg_live_through = 14`, `region_live` starts at 9 (`drain` drops it to 8 -> total
        // `22 >= TOTAL_GPR_BUDGET`, `< LOAD_HOIST_PRESSURE_LIMIT`). A strictly draining op
        // (`delta = -1`) still runs first, then a ready non-exit `Op::Load` (`delta = +1`)
        // hoists ahead of `delta == 0` bitwise `And`/`Or` ALU ops (including at `total == 26`,
        // `seg_live_through = 18`). At `seg_live_through = 19` (total `27 >=
        // LOAD_HOIST_PRESSURE_LIMIT`), load hoisting backs off until pressure drains.
        let build_loop = |live_through: usize| -> Func<TestHeap> {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let tbl_slot = b.slot(8, 3, SlotKind::Temp);
            let out_slot = b.slot(8, 3, SlotKind::Temp);
            let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let tbl_p = b.slot_addr(tbl_slot);
            let out_p = b.slot_addr(out_slot);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let mask = b.iconst(Ty::I64, 0x00ff_ffff);
            let or_k1 = b.iconst(Ty::I64, 0x1357_9bdf);
            let or_k2 = b.iconst(Ty::I64, 0x2468_ace1);
            let tbl_init = b.binary(Op::Xor, Flags::NONE, seed, or_k1);
            b.store(Flags::NONE, tbl_init, tbl_p, 0, MemAttrs::aligned(3));
            let mut init_args: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through + 9, TestHeap);
            for i in 0..live_through {
                let off = b.iconst(Ty::I64, u128::try_from(i + 1).unwrap());
                init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
            }
            init_args.push(tbl_p);
            for i in 0u128..7 {
                let off = b.iconst(Ty::I64, 100 + i * 7);
                init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
            }
            init_args.push(n);
            let init_slice: &[crate::Value] = &init_args;
            b.jump((loop_blk, init_slice));

            b.begin(loop_blk);
            let mut live_params: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through, TestHeap);
            for _ in 0..live_through {
                live_params.push(b.param(Ty::I64));
            }
            let tbl_ptr = b.param(Ty::PTR);
            let a0 = b.param(Ty::I64);
            let a1 = b.param(Ty::I64);
            let b0 = b.param(Ty::I64);
            let b1 = b.param(Ty::I64);
            let b2 = b.param(Ty::I64);
            let extra = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let rem = b.param(Ty::I64);

            // Place `delta == 0` bitwise ALU ops before the ready `Op::Load` (`delta = +1`)
            // and the `delta == -1` draining `Xor` op.
            let alu_and = b.binary(Op::And, Flags::NONE, b0, mask);
            let alu_or1 = b.binary(Op::Or, Flags::NONE, b1, or_k1);
            let alu_or2 = b.binary(Op::Or, Flags::NONE, b2, or_k2);
            let loaded = b.load(Ty::I64, Flags::NONE, tbl_ptr, 0, MemAttrs::aligned(3));
            let drain = b.binary(Op::Xor, Flags::NONE, a0, a1);
            let mix_load = b.binary(Op::Xor, Flags::NONE, loaded, extra);
            let mix_alu = b.binary(Op::Add, Flags::NONE, alu_and, alu_or1);
            let mix_alu2 = b.binary(Op::Add, Flags::NONE, mix_alu, alu_or2);
            let mix_d = b.binary(Op::Add, Flags::NONE, mix_load, drain);
            let mix_all = b.binary(Op::Add, Flags::NONE, mix_alu2, mix_d);
            let final_sum = b.binary(Op::Add, Flags::NONE, acc, mix_all);
            let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let cont = b.icmp(IntCond::Ne, next_rem, c0);
            b.store(Flags::NONE, final_sum, out_p, 0, MemAttrs::aligned(3));

            let mut back_args: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through + 9, TestHeap);
            for &p in &live_params {
                back_args.push(p);
            }
            back_args.push(tbl_ptr);
            back_args.push(mix_alu);
            back_args.push(mix_alu2);
            back_args.push(mix_load);
            back_args.push(mix_d);
            back_args.push(mix_all);
            back_args.push(final_sum);
            back_args.push(final_sum);
            back_args.push(next_rem);
            let back_slice: &[crate::Value] = &back_args;
            b.br(Flags::NONE, cont, (loop_blk, back_slice), (exit_blk, &[final_sum]));

            b.begin(exit_blk);
            let out_sum = b.param(Ty::I64);
            let saved = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
            let mut res = b.binary(Op::Add, Flags::NONE, out_sum, saved);
            for &p in &live_params {
                res = b.binary(Op::Add, Flags::NONE, res, p);
            }
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        // 1. Moderate high pressure (`seg_live_through = 14`, `region_live = 9 -> 8`, total = 22
        //    >= `TOTAL_GPR_BUDGET`, `< LOAD_HOIST_PRESSURE_LIMIT`): `drain` (`delta = -1`) runs
        //    first, then `Op::Load` (`delta = +1`) hoists ahead of all `delta == 0` `And`/`Or`.
        let func = build_loop(14);
        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        assert_eq!(
            sv.op(seg_start),
            Op::Xor,
            "expected strictly draining Op::Xor (delta = -1) to schedule ahead of Op::Load"
        );
        assert_eq!(
            sv.op(seg_start + 1),
            Op::Load,
            "expected ready non-exit Op::Load to hoist ahead of delta == 0 And/Or ALU ops at total_live == 22"
        );
        let load_pos = seg_start + 1;
        let mut alu_count = 0usize;
        for inst in loop_range {
            if matches!(sv.op(inst), Op::And | Op::Or) {
                alu_count += 1;
                assert!(
                    load_pos < inst,
                    "expected Op::Load ({load_pos}) to schedule BEFORE delta == 0 {:?} ({inst})",
                    sv.op(inst)
                );
            }
        }
        assert_eq!(alu_count, 3);

        for &(nv, seed_v) in &[(1u64, 7u64), (3, 0x1234_5678), (5, 0xdead_beef)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(seed_v))];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "moderate-pressure load hoist schedule mismatch on (n={nv}, seed={seed_v:#x})"
            );
        }

        // 2. Boundary hoist check (`seg_live_through = 18`, `region_live = 9 -> 8`, total = 26
        //    < `LOAD_HOIST_PRESSURE_LIMIT`): `Op::Load` still hoists at `total_live == 26` so
        //    dual hash-table probes can both launch before bitwise ALU ops.
        let func_26 = build_loop(18);
        let sched_26 = schedule_dag(func_26.view(), TestHeap).unwrap();
        let sv_26 = sched_26.func.view();
        verify(sv_26).unwrap();
        let seg_start_26 = sv_26.params(Block(1)).end;
        assert_eq!(sv_26.op(seg_start_26), Op::Xor);
        assert_eq!(
            sv_26.op(seg_start_26 + 1),
            Op::Load,
            "expected ready non-exit Op::Load to still hoist at total_live == 26 (< LOAD_HOIST_PRESSURE_LIMIT)"
        );

        // 3. Pressure ceiling guard (`seg_live_through = 19`, `region_live = 9 -> 8`, total = 27
        //    >= `LOAD_HOIST_PRESSURE_LIMIT`): `Op::Load` must NOT hoist ahead of `delta == 0`
        //    ALU until pressure drops below `LOAD_HOIST_PRESSURE_LIMIT`.
        let func_hi = build_loop(19);
        let sched_hi = schedule_dag(func_hi.view(), TestHeap).unwrap();
        let sv_hi = sched_hi.func.view();
        verify(sv_hi).unwrap();

        let seg_start_hi = sv_hi.params(Block(1)).end;
        let mut load_pos_hi = None;
        let mut first_alu_hi = None;
        for inst in sv_hi.block(Block(1)) {
            match sv_hi.op(inst) {
                Op::Load => load_pos_hi = Some(inst),
                Op::And | Op::Or if first_alu_hi.is_none() => first_alu_hi = Some(inst),
                _ => {}
            }
        }
        assert_eq!(sv_hi.op(seg_start_hi), Op::Xor);
        assert!(
            first_alu_hi.unwrap() < load_pos_hi.unwrap(),
            "expected delta == 0 ALU ({}) to stay ahead of Op::Load ({}) at total_live >= LOAD_HOIST_PRESSURE_LIMIT",
            first_alu_hi.unwrap(),
            load_pos_hi.unwrap()
        );
    }

    #[test]
    fn schedule_dag_hoists_load_and_ptradd_ahead_of_zero_delta_alu_under_moderate_pressure() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `lz77_scan_default_leaf`'s `shl -> ptr_add -> load` hash-table lookup under
        // moderate high pressure (`TOTAL_GPR_BUDGET <= total_live < LOAD_HOIST_PRESSURE_LIMIT`)
        // when `Shl` and `PtrAdd` are multi-use (non-folded, so `Shl`, `PtrAdd`, and `Load` each
        // have `delta == +1`): the address-forming chain (`is_load_addr`) and `Op::Load` hoist
        // ahead of `delta == 0` bitwise `And`/`Or` ALU ops, and stop hoisting once
        // `total_live >= LOAD_HOIST_PRESSURE_LIMIT`. (Single-use folded `Shl -> PtrAdd -> Load`
        // chains are tested in
        // `schedule_dag_folded_shl_ptradd_does_not_inflate_pressure_and_hoists_second_table_load`.)
        let build_loop = |live_through: usize| -> Func<TestHeap> {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let tbl_slot = b.slot(64, 3, SlotKind::Temp);
            let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let tbl_p = b.slot_addr(tbl_slot);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c3 = b.iconst(Ty::I64, 3);
            let c7 = b.iconst(Ty::I64, 7);
            let mask = b.iconst(Ty::I64, 0x00ff_ffff);
            let or_k1 = b.iconst(Ty::I64, 0x1357_9bdf);
            let or_k2 = b.iconst(Ty::I64, 0x2468_ace1);
            for k in 0u128..8 {
                let off_v = b.iconst(Ty::I64, (k + 1) * 0x0102_0304);
                let init_v = b.binary(Op::Xor, Flags::NONE, seed, off_v);
                let off = i32::try_from(k * 8).unwrap();
                b.store(Flags::NONE, init_v, tbl_p, off, MemAttrs::aligned(3));
            }
            let init_idx = b.binary(Op::And, Flags::NONE, seed, c7);
            let mut init_args: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through + 9, TestHeap);
            for i in 0..live_through {
                let off = b.iconst(Ty::I64, u128::try_from(i + 1).unwrap());
                init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
            }
            init_args.push(tbl_p);
            init_args.push(init_idx);
            for i in 0u128..6 {
                let off = b.iconst(Ty::I64, 100 + i * 7);
                init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
            }
            init_args.push(n);
            let init_slice: &[crate::Value] = &init_args;
            b.jump((loop_blk, init_slice));

            b.begin(loop_blk);
            let mut live_params: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through, TestHeap);
            for _ in 0..live_through {
                live_params.push(b.param(Ty::I64));
            }
            let tbl_ptr = b.param(Ty::PTR);
            let idx = b.param(Ty::I64);
            let a0 = b.param(Ty::I64);
            let a1 = b.param(Ty::I64);
            let b0 = b.param(Ty::I64);
            let b1 = b.param(Ty::I64);
            let b2 = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let rem = b.param(Ty::I64);

            // Place `delta == 0` bitwise ALU ops before the `Shl -> PtrAdd -> Load` chain
            // (all three have `delta == +1`) and the `delta == -1` draining `Xor` op.
            let alu_and = b.binary(Op::And, Flags::NONE, b0, mask);
            let alu_or1 = b.binary(Op::Or, Flags::NONE, b1, or_k1);
            let alu_or2 = b.binary(Op::Or, Flags::NONE, b2, or_k2);
            let byte_off = b.binary(Op::Shl, Flags::NONE, idx, c3);
            let elem_ptr = b.ptr_add(Flags::NONE, tbl_ptr, byte_off);
            let loaded = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(3));
            let drain = b.binary(Op::Xor, Flags::NONE, a0, a1);
            let mix_load = b.binary(Op::Xor, Flags::NONE, loaded, byte_off);
            let mix_alu = b.binary(Op::Add, Flags::NONE, alu_and, alu_or1);
            let mix_alu2 = b.binary(Op::Add, Flags::NONE, mix_alu, alu_or2);
            let mix_d = b.binary(Op::Add, Flags::NONE, mix_load, drain);
            let mix_all = b.binary(Op::Add, Flags::NONE, mix_alu2, mix_d);
            let final_sum = b.binary(Op::Add, Flags::NONE, acc, mix_all);
            let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let cont = b.icmp(IntCond::Ne, next_rem, c0);
            b.store(Flags::NONE, final_sum, elem_ptr, 0, MemAttrs::aligned(3));

            let mut back_args: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through + 9, TestHeap);
            for &p in &live_params {
                back_args.push(p);
            }
            back_args.push(tbl_ptr);
            back_args.push(idx);
            back_args.push(mix_alu);
            back_args.push(mix_alu2);
            back_args.push(mix_load);
            back_args.push(mix_d);
            back_args.push(mix_all);
            back_args.push(final_sum);
            back_args.push(next_rem);
            let back_slice: &[crate::Value] = &back_args;
            b.br(Flags::NONE, cont, (loop_blk, back_slice), (exit_blk, &[final_sum]));

            b.begin(exit_blk);
            let out_sum = b.param(Ty::I64);
            let saved = b.load(Ty::I64, Flags::NONE, tbl_p, 0, MemAttrs::aligned(3));
            let mut res = b.binary(Op::Add, Flags::NONE, out_sum, saved);
            for &p in &live_params {
                res = b.binary(Op::Add, Flags::NONE, res, p);
            }
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        // 1. Moderate high pressure (`seg_live_through = 14`, `region_live = 9 -> 8`, total = 22
        //    -> 23 -> 24 -> 25, all `< LOAD_HOIST_PRESSURE_LIMIT`): `drain` (`delta = -1`) runs
        //    first, then `Shl`, `PtrAdd`, and `Op::Load` all hoist ahead of `delta == 0` `And`/`Or`.
        let func = build_loop(14);
        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        assert_eq!(
            [
                sv.op(seg_start),
                sv.op(seg_start + 1),
                sv.op(seg_start + 2),
                sv.op(seg_start + 3),
            ],
            [Op::Xor, Op::Shl, Op::PtrAdd, Op::Load],
            "expected drain Xor followed by hoisted Shl -> PtrAdd -> Load ahead of delta == 0 And/Or"
        );
        let load_pos = seg_start + 3;
        let mut alu_count = 0usize;
        for inst in loop_range {
            if matches!(sv.op(inst), Op::And | Op::Or) {
                alu_count += 1;
                assert!(
                    load_pos < inst,
                    "expected Shl/PtrAdd/Load (load at {load_pos}) to schedule BEFORE delta == 0 {:?} ({inst})",
                    sv.op(inst)
                );
            }
        }
        assert_eq!(alu_count, 3);

        for &(nv, seed_v) in &[(1u64, 7u64), (3, 0x1234_5678), (5, 0xdead_beef)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(seed_v))];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "moderate-pressure ptr_add+load hoist schedule mismatch on (n={nv}, seed={seed_v:#x})"
            );
        }

        // 2. Boundary hoist check (`seg_live_through = 16`, `region_live = 9 -> 8`, total = 24
        //    -> 25 -> 26, all `< LOAD_HOIST_PRESSURE_LIMIT`): `Shl` (at 24), `PtrAdd` (at 25),
        //    and `Op::Load` (at `total_live == 26`) still hoist ahead of `delta == 0` `And`/`Or`.
        let func_26 = build_loop(16);
        let sched_26 = schedule_dag(func_26.view(), TestHeap).unwrap();
        let sv_26 = sched_26.func.view();
        verify(sv_26).unwrap();
        let seg_start_26 = sv_26.params(Block(1)).end;
        assert_eq!(
            [
                sv_26.op(seg_start_26),
                sv_26.op(seg_start_26 + 1),
                sv_26.op(seg_start_26 + 2),
                sv_26.op(seg_start_26 + 3),
            ],
            [Op::Xor, Op::Shl, Op::PtrAdd, Op::Load],
            "expected Shl -> PtrAdd -> Load to still hoist with Load launching at total_live == 26 (< LOAD_HOIST_PRESSURE_LIMIT)"
        );

        // 3. Pressure ceiling guard (`seg_live_through = 19`, `region_live = 9 -> 8`, total = 27
        //    >= `LOAD_HOIST_PRESSURE_LIMIT`): `Shl`, `PtrAdd`, and `Op::Load` must NOT hoist ahead
        //    of `delta == 0` ALU until pressure drops below `LOAD_HOIST_PRESSURE_LIMIT`.
        let func_hi = build_loop(19);
        let sched_hi = schedule_dag(func_hi.view(), TestHeap).unwrap();
        let sv_hi = sched_hi.func.view();
        verify(sv_hi).unwrap();

        let seg_start_hi = sv_hi.params(Block(1)).end;
        let mut shl_pos_hi = None;
        let mut ptr_add_pos_hi = None;
        let mut load_pos_hi = None;
        let mut first_alu_hi = None;
        for inst in sv_hi.block(Block(1)) {
            match sv_hi.op(inst) {
                Op::Shl => shl_pos_hi = Some(inst),
                Op::PtrAdd => ptr_add_pos_hi = Some(inst),
                Op::Load => load_pos_hi = Some(inst),
                Op::And | Op::Or if first_alu_hi.is_none() => first_alu_hi = Some(inst),
                _ => {}
            }
        }
        assert_eq!(sv_hi.op(seg_start_hi), Op::Xor);
        let first_alu = first_alu_hi.unwrap();
        let shl_pos = shl_pos_hi.unwrap();
        let ptr_add_pos = ptr_add_pos_hi.unwrap();
        let load_pos = load_pos_hi.unwrap();
        assert!(
            first_alu < shl_pos && shl_pos < ptr_add_pos && ptr_add_pos < load_pos,
            "expected delta == 0 ALU ({first_alu}) to stay ahead of Shl ({shl_pos}) -> PtrAdd ({ptr_add_pos}) -> Load ({load_pos}) at total_live >= LOAD_HOIST_PRESSURE_LIMIT"
        );
    }

    #[test]
    fn schedule_dag_clusters_select_families_and_prioritizes_zero_eq() {
        // Model Zstd's repeat-offset balanced select tree inside a loop:
        //   rep_lo  = Select(rep_code == 0, r0, r1)         [non-exit]
        //   next_r1 = Select(rep_code == 0, r1, r0)         [exit-only, replaces r1!]
        //   rep_hi  = Select(rep_code == 2, r2, r0 - 1)     [non-exit, delta == 0]
        //   rep_off = Select(rep_code <u 2, rep_lo, rep_hi) [non-exit]
        //   next_r2 = Select(rep_code <u 2, r2, r1)         [exit-only, reads r1!]
        // Verify:
        //   1. `rep_lo` and `next_r1` (family `(rep_code, 0)`) schedule contiguously BEFORE
        //      `(rep_code, 2)` selects (even though `next_r1` replaces `r1` which `next_r2` reads).
        //   2. `rep_hi`, `rep_off`, and `next_r2` (family `(rep_code, 2)`) schedule contiguously
        //      with zero intervening `rep_code == 0` select.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, loop_b, exit_b] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let r0_init = b.param(Ty::I64);
        let r1_init = b.param(Ty::I64);
        let r2_init = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        b.jump((loop_b, &[c0, r0_init, r1_init, r2_init][..]));

        b.begin(loop_b);
        let i = b.param(Ty::I64);
        let r0 = b.param(Ty::I64);
        let r1 = b.param(Ty::I64);
        let r2 = b.param(Ty::I64);
        let rep_code = b.binary(Op::And, Flags::NONE, i, c3);
        let r0_m1 = b.binary(Op::Sub, Flags::NONE, r0, c1);
        let is_eq0 = b.icmp(IntCond::Eq, rep_code, c0);
        let is_eq2 = b.icmp(IntCond::Eq, rep_code, c2);
        let is_lt2 = b.icmp(IntCond::Ult, rep_code, c2);
        // Deliberately emit in an interleaved order (eq2, eq0, lt2, lt2, eq0).
        let rep_hi = b.select(Flags::NONE, is_eq2, r2, r0_m1);
        let rep_lo = b.select(Flags::NONE, is_eq0, r0, r1);
        let rep_off = b.select(Flags::NONE, is_lt2, rep_lo, rep_hi);
        let next_r2 = b.select(Flags::NONE, is_lt2, r2, r1);
        let next_r1 = b.select(Flags::NONE, is_eq0, r1, r0);
        let next_r0 = b.binary(Op::Add, Flags::NONE, rep_off, c1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, c1);
        let keep = b.icmp(IntCond::Ult, next_i, n);
        b.br(
            Flags::NONE,
            keep,
            (loop_b, &[next_i, next_r0, next_r1, next_r2][..]),
            (exit_b, &[next_r0, next_r1, next_r2][..]),
        );

        b.begin(exit_b);
        let er0 = b.param(Ty::I64);
        let er1 = b.param(Ty::I64);
        let er2 = b.param(Ty::I64);
        let s01 = b.binary(Op::Add, Flags::NONE, er0, er1);
        let sum = b.binary(Op::Add, Flags::NONE, s01, er2);
        b.ret(&[sum]);

        let func = b.finish().unwrap();
        verify(func.view()).unwrap();
        let out = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = out.func.view();
        verify(sv).unwrap();

        let mut select_fams: Vec<bool, TestHeap> = Vec::new_in(TestHeap);
        for inst in sv.block(Block(1)) {
            if let Some((_, _, is_zero_eq)) = select_cmp_info(&sv, inst) {
                select_fams.push(is_zero_eq);
            }
        }
        assert_eq!(
            select_fams.as_slice(),
            &[true, true, false, false, false],
            "expected both x==0 selects contiguously before all three (x, 2) selects, got {:?}",
            select_fams.as_slice()
        );

        let args = [Val::Int(12), Val::Int(10), Val::Int(20), Val::Int(30)];
        let Outcome::Returned(r_before) = eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
        else {
            panic!()
        };
        let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap() else {
            panic!()
        };
        assert_eq!(r_before, r_after);
    }

    #[test]
    fn schedule_dag_folded_shl_ptradd_does_not_inflate_pressure_and_hoists_second_table_load() {
        use crate::{
            IntCond,
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `lz77_scan_default_leaf` Lane 0's dual L1/L2 hash-table probes where each
        // single-use `Shl(idx, 2) -> PtrAdd(table, shl) -> Load(ptr)` lowers on AArch64 to a
        // single `[base, idx, lsl #2]` instruction with zero intermediate physical registers.
        // Without `is_folded_addr`, both `Shl`s bump `total_live` from `25 -> 26 -> 27`, tripping
        // `LOAD_HOIST_PRESSURE_LIMIT` (27) before either `PtrAdd`/`Load` runs. With
        // `is_folded_addr`, `Shl` and `PtrAdd` keep `total_live` flat (`25 -> 25 -> 25 -> 26 ->
        // 26 -> 27`), so BOTH L1 and L2 table loads hoist ahead of `delta == 0` `And`/`Or` ALU.
        let build_dual_table_loop = |live_through: usize| -> Func<TestHeap> {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let tbl1_slot = b.slot(32, 2, SlotKind::Temp);
            let tbl2_slot = b.slot(32, 2, SlotKind::Temp);
            let out_slot = b.slot(8, 3, SlotKind::Temp);
            let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let seed32 = b.convert(Op::Trunc, Flags::NONE, Ty::I32, seed);
            let tbl1_p = b.slot_addr(tbl1_slot);
            let tbl2_p = b.slot_addr(tbl2_slot);
            let out_p = b.slot_addr(out_slot);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            let c3 = b.iconst(Ty::I64, 3);
            let c7 = b.iconst(Ty::I64, 7);
            let mask = b.iconst(Ty::I64, 0x00ff_ffff);
            let or_k1 = b.iconst(Ty::I64, 0x1357_9bdf);
            let or_k2 = b.iconst(Ty::I64, 0x2468_ace1);
            for k in 0u128..8 {
                let off_v1 = b.iconst(Ty::I32, (k + 1) * 0x0102_0304);
                let init_v1 = b.binary(Op::Xor, Flags::NONE, seed32, off_v1);
                let off_v2 = b.iconst(Ty::I32, (k + 11) * 0x0506_0708);
                let init_v2 = b.binary(Op::Xor, Flags::NONE, seed32, off_v2);
                let off = i32::try_from(k * 4).unwrap();
                b.store(Flags::NONE, init_v1, tbl1_p, off, MemAttrs::aligned(2));
                b.store(Flags::NONE, init_v2, tbl2_p, off, MemAttrs::aligned(2));
            }
            let init_idx1 = b.binary(Op::And, Flags::NONE, seed, c7);
            let shifted_seed = b.binary(Op::Lshr, Flags::NONE, seed, c3);
            let init_idx2 = b.binary(Op::And, Flags::NONE, shifted_seed, c7);
            let mut init_args: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through + 9, TestHeap);
            for i in 0..live_through {
                let off = b.iconst(Ty::I64, u128::try_from(i + 1).unwrap());
                init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
            }
            init_args.push(tbl1_p);
            init_args.push(tbl2_p);
            init_args.push(init_idx1);
            init_args.push(init_idx2);
            for i in 0u128..4 {
                let off = b.iconst(Ty::I64, 100 + i * 7);
                init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
            }
            init_args.push(n);
            let init_slice: &[crate::Value] = &init_args;
            b.jump((loop_blk, init_slice));

            b.begin(loop_blk);
            let mut live_params: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through, TestHeap);
            for _ in 0..live_through {
                live_params.push(b.param(Ty::I64));
            }
            let long_tbl = b.param(Ty::PTR);
            let short_tbl = b.param(Ty::PTR);
            let idx1 = b.param(Ty::I64);
            let idx2 = b.param(Ty::I64);
            let b0 = b.param(Ty::I64);
            let b1 = b.param(Ty::I64);
            let b2 = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let rem = b.param(Ty::I64);

            // Place `delta == 0` bitwise ALU ops ahead of both single-use folded
            // `Shl(idx, 2) -> PtrAdd(tbl, shl) -> Load(ptr)` table lookups.
            let alu_and = b.binary(Op::And, Flags::NONE, b0, mask);
            let alu_or1 = b.binary(Op::Or, Flags::NONE, b1, or_k1);
            let alu_or2 = b.binary(Op::Or, Flags::NONE, b2, or_k2);
            let shl1 = b.binary(Op::Shl, Flags::NONE, idx1, c2);
            let ptr1 = b.ptr_add(Flags::NONE, long_tbl, shl1);
            let load1 = b.load(Ty::I32, Flags::NONE, ptr1, 0, MemAttrs::aligned(2));
            let load1_64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, load1);
            let shl2 = b.binary(Op::Shl, Flags::NONE, idx2, c2);
            let ptr2 = b.ptr_add(Flags::NONE, short_tbl, shl2);
            let load2 = b.load(Ty::I32, Flags::NONE, ptr2, 0, MemAttrs::aligned(2));
            let load2_64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, load2);
            let mix_loads = b.binary(Op::Xor, Flags::NONE, load1_64, load2_64);
            let mix_alu = b.binary(Op::Add, Flags::NONE, alu_and, alu_or1);
            let mix_alu2 = b.binary(Op::Add, Flags::NONE, mix_alu, alu_or2);
            let mix_all = b.binary(Op::Add, Flags::NONE, mix_alu2, mix_loads);
            let final_sum = b.binary(Op::Add, Flags::NONE, acc, mix_all);
            let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let cont = b.icmp(IntCond::Ne, next_rem, c0);
            b.store(Flags::NONE, final_sum, out_p, 0, MemAttrs::aligned(3));

            let mut back_args: Vec<crate::Value, TestHeap> =
                Vec::with_capacity_in(live_through + 9, TestHeap);
            for &p in &live_params {
                back_args.push(p);
            }
            back_args.push(long_tbl);
            back_args.push(short_tbl);
            back_args.push(idx1);
            back_args.push(idx2);
            back_args.push(mix_alu);
            back_args.push(mix_alu2);
            back_args.push(mix_loads);
            back_args.push(final_sum);
            back_args.push(next_rem);
            let back_slice: &[crate::Value] = &back_args;
            b.br(Flags::NONE, cont, (loop_blk, back_slice), (exit_blk, &[final_sum]));

            b.begin(exit_blk);
            let out_sum = b.param(Ty::I64);
            let saved = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
            let mut res = b.binary(Op::Add, Flags::NONE, out_sum, saved);
            for &p in &live_params {
                res = b.binary(Op::Add, Flags::NONE, res, p);
            }
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        // 1. `seg_live_through = 16`, `region_live = 9` -> `total_live = 25`:
        //    `shl1` (25), `shl2` (25), `ptr1` (25), `load1` (25 -> 26), `ptr2` (26),
        //    `load2` (26 -> 27) ALL hoist strictly ahead of every `delta == 0` `And`/`Or` ALU op!
        let func_25 = build_dual_table_loop(16);
        let sched_25 = schedule_dag(func_25.view(), TestHeap).unwrap();
        let sv_25 = sched_25.func.view();
        verify(sv_25).unwrap();

        let seg_start_25 = sv_25.params(Block(1)).end;
        assert_eq!(
            [
                sv_25.op(seg_start_25),
                sv_25.op(seg_start_25 + 1),
                sv_25.op(seg_start_25 + 2),
                sv_25.op(seg_start_25 + 3),
                sv_25.op(seg_start_25 + 4),
                sv_25.op(seg_start_25 + 5),
            ],
            [Op::Shl, Op::Shl, Op::PtrAdd, Op::Load, Op::PtrAdd, Op::Load],
            "expected both folded Shl -> PtrAdd -> Load table probes to hoist at total_live == 25..26 ahead of And/Or"
        );
        let second_load_pos = seg_start_25 + 5;
        let mut alu_count = 0usize;
        for inst in sv_25.block(Block(1)) {
            if matches!(sv_25.op(inst), Op::And | Op::Or) {
                alu_count += 1;
                assert!(
                    second_load_pos < inst,
                    "expected second hoisted table Op::Load ({second_load_pos}) BEFORE delta == 0 {:?} ({inst})",
                    sv_25.op(inst)
                );
            }
        }
        assert_eq!(alu_count, 3);

        for &(nv, seed_v) in &[(1u64, 7u64), (3, 0x1234_5678), (5, 0xdead_beef)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(seed_v))];
            let Outcome::Returned(r_before) =
                eval::run_in(func_25.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) =
                eval::run_in(sv_25, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "folded dual-table load hoist schedule mismatch on (n={nv}, seed={seed_v:#x})"
            );
        }

        // 2. `seg_live_through = 17`, `region_live = 9` -> `total_live = 26`:
        //    First folded chain hoists cleanly at `total_live == 26` (`26 -> 27` at `load1`),
        //    while `load2` (`delta == +1` at `total_live == 27`) waits behind `And`/`Or` until
        //    pressure drains back below `LOAD_HOIST_PRESSURE_LIMIT`.
        let func_26 = build_dual_table_loop(17);
        let sched_26 = schedule_dag(func_26.view(), TestHeap).unwrap();
        let sv_26 = sched_26.func.view();
        verify(sv_26).unwrap();
        let mut first_load_26 = None;
        let mut second_load_26 = None;
        let mut first_alu_26 = None;
        for inst in sv_26.block(Block(1)) {
            match sv_26.op(inst) {
                Op::Load if first_load_26.is_none() => first_load_26 = Some(inst),
                Op::Load => second_load_26 = Some(inst),
                Op::And | Op::Or if first_alu_26.is_none() => first_alu_26 = Some(inst),
                _ => {}
            }
        }
        let l1_26 = first_load_26.unwrap();
        let l2_26 = second_load_26.unwrap();
        let alu_26 = first_alu_26.unwrap();
        assert!(
            l1_26 < alu_26 && alu_26 < l2_26,
            "expected first folded Load ({l1_26}) to hoist at total_live == 26 ahead of ALU ({alu_26}), and second Load ({l2_26}) to wait at total_live == 27"
        );

        // 3. `seg_live_through = 18`, `region_live = 9` -> `total_live = 27 >=
        //    LOAD_HOIST_PRESSURE_LIMIT`: neither `Load` hoists ahead of the first `And`/`Or`.
        let func_27 = build_dual_table_loop(18);
        let sched_27 = schedule_dag(func_27.view(), TestHeap).unwrap();
        let sv_27 = sched_27.func.view();
        verify(sv_27).unwrap();
        let mut first_load_27 = None;
        let mut first_alu_27 = None;
        for inst in sv_27.block(Block(1)) {
            match sv_27.op(inst) {
                Op::Load if first_load_27.is_none() => first_load_27 = Some(inst),
                Op::And | Op::Or if first_alu_27.is_none() => first_alu_27 = Some(inst),
                _ => {}
            }
        }
        assert!(
            first_alu_27.unwrap() < first_load_27.unwrap(),
            "expected delta == 0 ALU ({}) to stay ahead of Op::Load ({}) at total_live >= LOAD_HOIST_PRESSURE_LIMIT",
            first_alu_27.unwrap(),
            first_load_27.unwrap()
        );
    }

    #[test]
    fn schedule_dag_does_not_defer_draining_exit_recurrence_under_high_pressure() {
        use crate::{
            builder::MemAttrs,
            func::SlotKind,
        };

        // Mirrors `decode_exec_leaf_single` (`bits_consumed` / `next_ml_state` loop-carried
        // recurrences): with `seg_live_through == 16` (`total_live >= EXIT_ALU_DEFER_PRESSURE`):
        // - draining (`delta == -1`: `next_bits = Add(bits_state, delta_bits)`) and neutral
        //   (`delta == 0`: `next_ml = Not(ml_state)`) `only_feeds_exit` recurrence ops whose
        //   inputs have no remaining non-exit uses must NOT defer behind unrelated ready non-exit
        //   work (`w0 = Or(work, c3)`), while
        // - pressure-increasing (`delta == +1`: `ext_hi0 = Mul(shared_in, c5)` reading live-out
        //   `shared_in`) `only_feeds_exit` ops MUST still defer strictly after the non-exit chain
        //   (`w0 -> w1 -> w2`), even when placed first in input order with higher latency height.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, loop_blk, exit_blk] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let out_p = b.slot_addr(out_slot);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c5 = b.iconst(Ty::I64, 5);
        let c8 = b.iconst(Ty::I64, 8);
        let mut init_args: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(23, TestHeap);
        for i in 0u128..16 {
            let off = b.iconst(Ty::I64, i + 1);
            init_args.push(b.binary(Op::Add, Flags::NONE, seed, off));
        }
        let shared_init = b.binary(Op::Add, Flags::NONE, seed, c8);
        let delta_init = b.binary(Op::Add, Flags::NONE, seed, c5);
        let ml_init = b.binary(Op::Add, Flags::NONE, seed, c3);
        let work_init = b.binary(Op::Add, Flags::NONE, seed, c1);
        init_args.push(shared_init);
        init_args.push(seed);
        init_args.push(delta_init);
        init_args.push(ml_init);
        init_args.push(work_init);
        init_args.push(c0);
        init_args.push(n);
        let init_slice: &[crate::Value] = &init_args;
        b.jump((loop_blk, init_slice));

        b.begin(loop_blk);
        let live_params: [crate::Value; 16] = core::array::from_fn(|_| b.param(Ty::I64));
        let shared_in = b.param(Ty::I64);
        let bits_state = b.param(Ty::I64);
        let delta_bits = b.param(Ty::I64);
        let ml_state = b.param(Ty::I64);
        let work = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);

        // Pressure-increasing (`delta == +1`) `only_feeds_exit` chain placed FIRST with higher
        // latency height (`Mul` = 3 + `Lshr` = 1 -> height 4 > `w0`'s height 3):
        let ext_hi0 = b.binary(Op::Mul, Flags::NONE, shared_in, c5);
        let ext_hi1 = b.binary(Op::Lshr, Flags::NONE, ext_hi0, c8);
        // Unrelated non-exit compute chain (`w0` has `delta == +1`, height 3) feeding `Store`:
        let w0 = b.binary(Op::Or, Flags::NONE, work, c3);
        let w1 = b.binary(Op::Xor, Flags::NONE, w0, work);
        let w2 = b.binary(Op::Umax, Flags::NONE, w1, acc);
        // Draining (`delta == -1`) and neutral (`delta == 0`) `only_feeds_exit` recurrence ops
        // placed AFTER `w0..w2` whose inputs have zero remaining non-exit uses:
        let next_bits = b.binary(Op::Add, Flags::NONE, bits_state, delta_bits);
        let next_ml = b.unary(Op::Not, Flags::NONE, ml_state);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let cont = b.icmp(IntCond::Ne, next_rem, c0);
        b.store(Flags::NONE, w2, out_p, 0, MemAttrs::aligned(3));

        let mut back_args: Vec<crate::Value, TestHeap> = Vec::with_capacity_in(23, TestHeap);
        for &p in &live_params {
            back_args.push(p);
        }
        back_args.push(shared_in);
        back_args.push(next_bits);
        back_args.push(w2);
        back_args.push(next_ml);
        back_args.push(w2);
        back_args.push(w2);
        back_args.push(next_rem);
        let back_slice: &[crate::Value] = &back_args;
        b.br(
            Flags::NONE,
            cont,
            (loop_blk, back_slice),
            (exit_blk, &[next_bits, next_ml, ext_hi1, w2]),
        );

        b.begin(exit_blk);
        let out_bits = b.param(Ty::I64);
        let out_ml = b.param(Ty::I64);
        let out_ext = b.param(Ty::I64);
        let out_w2 = b.param(Ty::I64);
        let saved = b.load(Ty::I64, Flags::NONE, out_p, 0, MemAttrs::aligned(3));
        let mut res = b.binary(Op::Add, Flags::NONE, out_bits, out_ml);
        res = b.binary(Op::Xor, Flags::NONE, res, out_ext);
        res = b.binary(Op::Add, Flags::NONE, res, out_w2);
        res = b.binary(Op::Add, Flags::NONE, res, saved);
        res = b.binary(Op::Add, Flags::NONE, res, shared_in);
        for &p in &live_params {
            res = b.binary(Op::Add, Flags::NONE, res, p);
        }
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let sched = schedule_dag(func.view(), TestHeap).unwrap();
        let sv = sched.func.view();
        verify(sv).unwrap();

        let loop_range = sv.block(Block(1));
        let seg_start = sv.params(Block(1)).end;
        let term = loop_range.end - 1;

        let mut next_bits_pos = None;
        let mut next_ml_pos = None;
        let mut w0_pos = None;
        let mut w2_pos = None;
        let mut ext_hi0_pos = None;
        let mut ext_hi1_pos = None;
        for inst in seg_start..term {
            match sv.op(inst) {
                Op::Add => next_bits_pos = Some(inst),
                Op::Not => next_ml_pos = Some(inst),
                Op::Or => w0_pos = Some(inst),
                Op::Umax => w2_pos = Some(inst),
                Op::Mul => ext_hi0_pos = Some(inst),
                Op::Lshr => ext_hi1_pos = Some(inst),
                _ => {}
            }
        }
        let next_bits_i = next_bits_pos.unwrap();
        let next_ml_i = next_ml_pos.unwrap();
        let w0_i = w0_pos.unwrap();
        let w2_i = w2_pos.unwrap();
        let ext_hi0_i = ext_hi0_pos.unwrap();
        let ext_hi1_i = ext_hi1_pos.unwrap();

        assert!(
            next_bits_i < w0_i,
            "expected draining (delta == -1) exit recurrence Op::Add ({next_bits_i}) to schedule BEFORE unrelated non-exit Op::Or ({w0_i})"
        );
        assert!(
            next_ml_i < w0_i,
            "expected neutral (delta == 0) exit recurrence Op::Not ({next_ml_i}) to schedule BEFORE pressure-increasing non-exit Op::Or ({w0_i})"
        );
        assert!(
            w2_i < ext_hi0_i && ext_hi0_i < ext_hi1_i,
            "expected pressure-increasing (delta == +1) exit-only Op::Mul ({ext_hi0_i}) -> Op::Lshr ({ext_hi1_i}) to remain deferred AFTER non-exit Op::Umax ({w2_i})"
        );

        for &(nv, seed_v) in &[(1u64, 7u64), (4, 0x1234_5678), (7, 0xdead_beef)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(seed_v))];
            let Outcome::Returned(r_before) =
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            let Outcome::Returned(r_after) = eval::run_in(sv, &args, 10_000, TestHeap).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                r_before, r_after,
                "draining exit-recurrence schedule mismatch on (n={nv}, seed={seed_v:#x})"
            );
        }
    }
}

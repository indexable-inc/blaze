//! Intra-Block Register-Pressure-Aware Critical-Path DAG List Scheduler (`blaze_lir::sched`).
//!
//! Reorders pure, side-effect-free instructions within each basic block (strictly between
//! `Op::Param`s and side-effecting barriers such as `Store`/`Call`/`Atomic*`/`Trap`/terminator)
//! using Sethi-Ullman / ILP-with-RegPressure list scheduling:
//!
//! - Under low register pressure (`region_live < REG_PRESSURE_HIGH`): prioritizes ready-now ops
//!   and highest bottom-up critical-path latency `height` — pulling long-latency `Load` (4 cyc)
//!   and `Umulhi`/`Mul`/`Fmul`/`Fadd`/`Fma` (3 cyc) ops forward and interleaving independent
//!   chains — breaking ties by smallest `live_delta`.
//! - Under high register pressure (`region_live >= REG_PRESSURE_HIGH`): prioritizes smallest
//!   `live_delta` first to drain live values before starting new independent chains, avoiding
//!   unnecessary register spills, breaking ties by highest `height` then original index.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, Flags, Func, Op, Ty, View,
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
        Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide => 0,
        _ => 1,
    }
}

/// High-pressure threshold for intra-segment live values.
/// Leaves headroom for outer-loop live-through values so total live GPRs stay <= 25 on AArch64.
const REG_PRESSURE_HIGH: usize = 16;

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
fn counts_for_pressure(ty: Ty) -> bool {
    !ty.is_none()
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

#[inline]
fn calc_live_delta(
    cand: usize,
    inst_defines_live: &[bool],
    op_starts: &[usize],
    inst_ops: &[usize],
    seg_rem_uses: &[u32],
    seg_live_out: &[bool],
) -> i32 {
    let mut delta: i32 = if at_bool(inst_defines_live, cand) { 1 } else { 0 };
    let start = at_usize(op_starts, cand);
    let end = at_usize(op_starts, cand + 1);
    for idx in start..end {
        let vi = at_usize(inst_ops, idx);
        if at_u32(seg_rem_uses, vi) == 1 && !at_bool(seg_live_out, vi) {
            delta -= 1;
        }
    }
    delta
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
    // its last using instruction index in that block, and whether the CFG has any backedge.
    let mut use_block = filled(len, NONE, &alloc);
    let mut last_use_inst = filled(len, 0u32, &alloc);
    let mut has_loop = false;
    for b in 0..blocks {
        let b32 = to_u32(b);
        let range = view.block(Block(b32));
        for inst in range.start..range.end {
            each(&view, inst, |item| match item {
                Item::Value(v) => record_use(v, b32, inst, &mut use_block, &mut last_use_inst),
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

    // Reusable per-segment pressure tables indexed by SSA value id (O(1) lookup, stamped reset).
    let mut seg_rem_uses = filled(len, 0u32, &alloc);
    let mut seg_live_out = filled(len, false, &alloc);
    let mut seg_stamp = filled(len, 0u32, &alloc);
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
            // Skip trivial or huge segments (keeps compile time strictly bounded & O(N) on huge blocks).
            if seg_len < 4 || seg_len > 96 {
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
            let mut inst_defines_live = filled(seg_len, false, &alloc);
            let mut op_starts: Vec<usize, A> = Vec::with_capacity_in(seg_len + 1, alloc);
            let mut inst_ops: Vec<usize, A> = Vec::with_capacity_in(seg_len * 2, alloc);
            let mut region_live = 0usize;

            for i in 0..seg_len {
                let orig = seg_start + i;
                succs.push(Vec::new_in(alloc));
                lats.push(op_latency(view.op(orig)));
                if counts_for_pressure(Ty(at_u16(view.tys, orig)))
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
                            if let Some(sp) = succs.get_mut(p) {
                                if sp.scan_for(&i).is_none() {
                                    sp.push(i);
                                    if let Some(pr) = pred_rem.get_mut(i) {
                                        *pr += 1;
                                    }
                                }
                            }
                        }
                        if counts_for_pressure(Ty(at_u16(view.tys, vi))) {
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

            // Keep loads ordered with respect to any preceding load only if they share no benefit?
            // No: loads in a pure segment have no intervening stores, so reordering loads is 100% sound!
            let mut height = filled(seg_len, 0u32, &alloc);
            for i in (0..seg_len).rev() {
                let mut max_s = 0u32;
                if let Some(si) = succs.get(i) {
                    for &s in si {
                        let hs = at_u32(&height, s);
                        if hs > max_s {
                            max_s = hs;
                        }
                    }
                }
                set_at(&mut height, i, at_u32(&lats, i) + max_s);
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
                let high_pressure = region_live >= REG_PRESSURE_HIGH;
                let mut best_idx = 0usize;
                let first_node = at_usize(&ready, 0);
                let mut best_delta = calc_live_delta(
                    first_node,
                    &inst_defines_live,
                    &op_starts,
                    &inst_ops,
                    &seg_rem_uses,
                    &seg_live_out,
                );
                for k in 1..ready.len() {
                    let a = at_usize(&ready, k);
                    let b_node = at_usize(&ready, best_idx);
                    let da = calc_live_delta(
                        a,
                        &inst_defines_live,
                        &op_starts,
                        &inst_ops,
                        &seg_rem_uses,
                        &seg_live_out,
                    );
                    let db = best_delta;
                    let ha = at_u32(&height, a);
                    let hb = at_u32(&height, b_node);
                    let better = if high_pressure {
                        // High register pressure: prioritize smallest `live_delta` (draining
                        // values instead of fanning out multi-use packed words), breaking ties
                        // by highest `height` then original index.
                        if da != db {
                            da < db
                        } else if ha != hb {
                            ha > hb
                        } else {
                            a < b_node
                        }
                    } else {
                        // Low register pressure: prefer ready-now (`ready_at <= cycle`) and
                        // highest critical-path `height`, breaking ties by smallest `live_delta`,
                        // earliest `ready_at`, then original index (deterministic).
                        let ra = at_u32(&ready_at, a);
                        let rb = at_u32(&ready_at, b_node);
                        let a_now = ra <= cycle;
                        let b_now = rb <= cycle;
                        if a_now != b_now {
                            a_now
                        } else if ha != hb {
                            ha > hb
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
                    }
                }
                let pick = ready.remove_swap(best_idx);
                if pick != emitted_pos {
                    block_reordered = true;
                    stats.insts_reordered += 1;
                }
                emitted_pos += 1;
                let orig = seg_start + pick;
                let mapped = copy_remapped(&view, orig, &val_map, &mut func);
                set_at(&mut val_map, orig, mapped);

                // Update `region_live` and `seg_rem_uses` for the scheduled instruction.
                if at_bool(&inst_defines_live, pick) {
                    region_live += 1;
                }
                let op_s = at_usize(&op_starts, pick);
                let op_e = at_usize(&op_starts, pick + 1);
                for idx in op_s..op_e {
                    let vi = at_usize(&inst_ops, idx);
                    if let Some(rem) = seg_rem_uses.get_mut(vi) {
                        if *rem > 0 {
                            *rem -= 1;
                            if *rem == 0 && !at_bool(&seg_live_out, vi) {
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

    verify(func.view()).map_err(SimplifyError::Verify)?;
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
}

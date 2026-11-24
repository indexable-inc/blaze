//! Intra-Block Critical-Path Latency-Aware DAG List Scheduler (`blaze_lir::sched`).
//!
//! Reorders pure, side-effect-free instructions within each basic block (strictly between
//! `Op::Param`s and side-effecting barriers such as `Store`/`Call`/`Atomic*`/`Trap`/terminator)
//! using bottom-up critical-path latency priority:
//!
//! - Pulls long-latency `Load` (4 cyc), `Umulhi`/`Mul`/`Fmul`/`Fadd`/`Fma` (3 cyc) ops forward.
//! - Interleaves independent computation chains (such as 4x-unrolled slices and crypto rounds)
//!   so out-of-order and in-order execution pipes stay saturated without changing semantics or
//!   memory ordering.
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
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
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

/// Schedules pure instruction segments inside every basic block by critical-path latency height.
pub fn schedule_dag<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<SchedOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if len < 4 {
        return Ok(SchedOutput { func: view.to_func_in(alloc), stats: SchedStats::default() });
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

            // Build intra-segment SSA dependency DAG (`pred_rem[i]`, `succs[i]`, `height[i]`).
            let mut pred_rem = filled(seg_len, 0u32, &alloc);
            let mut succs: Vec<Vec<usize, A>, A> = Vec::with_capacity_in(seg_len, alloc);
            let mut lats: Vec<u32, A> = Vec::with_capacity_in(seg_len, alloc);
            for i in 0..seg_len {
                succs.push(Vec::new_in(alloc));
                lats.push(op_latency(view.op(seg_start + i)));
            }
            for i in 0..seg_len {
                let inst = seg_start + i;
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
                    }
                });
            }

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
                // Pick best ready node: prefer already-ready (`ready_at <= cycle`), then highest
                // critical-path `height`, then earliest `ready_at`, then original index (deterministic!).
                let mut best_idx = 0usize;
                for k in 1..ready.len() {
                    let a = at_usize(&ready, k);
                    let b_node = at_usize(&ready, best_idx);
                    let ra = at_u32(&ready_at, a);
                    let rb = at_u32(&ready_at, b_node);
                    let ha = at_u32(&height, a);
                    let hb = at_u32(&height, b_node);
                    let a_now = ra <= cycle;
                    let b_now = rb <= cycle;
                    let better = if a_now != b_now {
                        a_now
                    } else if ha != hb {
                        ha > hb
                    } else if ra != rb {
                        ra < rb
                    } else {
                        a < b_node
                    };
                    if better {
                        best_idx = k;
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
}

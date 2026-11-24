//! Hot/Cold Basic-Block Partitioning and Cold-Block Outlining (`blaze_lir::coldsplit`).
//!
//! Identifies cold basic blocks (traps, unreachables, unwind landing pads, cold/panic callees,
//! unlikely branch targets, blocks post-dominated by cold paths, and blocks reachable only from
//! cold paths) and packs all hot blocks contiguously at the start of the function with all cold
//! blocks sunk contiguously to the tail.
//!
//! Packing hot blocks together maximizes L1 instruction-cache density and branch-predictor
//! locality, while placing all cold blocks in a single trailing region makes `.text.cold`
//! section splitting straightforward in downstream codegen.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, Op, Ty, View,
    op::{Field, Layout},
    verify,
};

/// Telemetry returned by [`partition_cold_blocks`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct ColdSplitStats {
    pub cold_blocks_identified: u32,
    pub blocks_sunk: u32,
}

/// Output of [`partition_cold_blocks`].
#[derive(Debug)]
pub struct ColdSplitOutput<A: Allocator> {
    pub func: Func<A>,
    /// Per block in `func`'s reordered layout: `true` iff the block was partitioned into the cold
    /// tail region.
    pub is_cold: Vec<bool, A>,
    pub stats: ColdSplitStats,
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
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
fn at<T: Copy>(s: &[T], i: usize, default: T) -> T {
    s.get(i).copied().unwrap_or(default)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, val: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = val;
    }
}

/// Copies instruction `inst` of `view` into `func`, remapping all value operands via `val_map`
/// and all block operands via `block_map`.
fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    block_map: &[u32],
    func: &mut Func<A>,
) -> Result<(), SimplifyError> {
    let op = view.op(inst);
    let flags = at(view.flags, inst, 0);
    let ty = at(view.tys, inst, 0);
    let attr = at(view.attrs, inst, 0);
    let loc = at(view.locs, inst, 0);
    let map_val = |v: u32| -> Result<u32, SimplifyError> {
        match at(val_map, to_idx(v), NONE) {
            NONE => Err(SimplifyError::Refused { inst, what: "a use of an unmapped value" }),
            mapped => Ok(mapped),
        }
    };
    let map_block = |b: u32| -> Result<u32, SimplifyError> {
        match at(block_map, to_idx(b), NONE) {
            NONE => Err(SimplifyError::Refused { inst, what: "an edge to an unmapped block" }),
            mapped => Ok(mapped),
        }
    };
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => map_val(at(view.data, inst, 0))?,
        Layout::Data(Field::Block) => map_block(at(view.data, inst, 0))?,
        Layout::Data(_) => at(view.data, inst, 0),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst, 0));
            let dst = u32_of(func.extra.len());
            let mut p = src;
            let mut scrutinee = None;
            for &f in fields {
                match f {
                    Field::Value => {
                        let v = at(view.extra, p, 0);
                        scrutinee = Some(v);
                        func.extra.push(map_val(v)?);
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(map_block(at(view.extra, p, 0))?);
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(map_val(at(view.extra, p + k, 0))?);
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(map_block(at(view.extra, p + k, 0))?);
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(map_block(at(view.extra, p, 0))?);
                        let n_u32 = at(view.extra, p + 1, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(map_val(at(view.extra, p + k, 0))?);
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let w = to_idx(scrutinee.map_or(0, |s| Ty(at(view.tys, to_idx(s), 0)).const_words()));
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k, 0));
                            }
                            p += w;
                            func.extra.push(map_block(at(view.extra, p, 0))?);
                            let an_u32 = at(view.extra, p + 1, 0);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(map_val(at(view.extra, p + k, 0))?);
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k, 0));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k, 0));
                        }
                        p += w;
                    }
                    Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(at(view.extra, p, 0));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    func.ops.push(at(view.ops, inst, 0));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    Ok(())
}

/// Deterministically identifies cold basic blocks in `view` and partitions the function layout so
/// all hot blocks appear contiguously first (entry block `0` always first, preserving relative
/// order) and all cold blocks are packed contiguously at the tail.
///
/// The resulting function is verified with [`verify`] before being returned.
pub fn partition_cold_blocks<A: Allocator + Copy>(
    view: View<'_>,
    is_cold_callee: impl Fn(u32) -> bool,
    alloc: A,
) -> Result<ColdSplitOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks <= 1 {
        return Ok(ColdSplitOutput {
            func: view.to_func_in(alloc),
            is_cold: filled(blocks, false, &alloc),
            stats: ColdSplitStats::default(),
        });
    }

    // Build CFG predecessors and successors in deterministic block/edge order.
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut in_edges = filled(blocks, 0u32, &alloc);
    for _ in 0..blocks {
        succs.push(Vec::new_in(alloc));
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = u32_of(b);
        let term = view.terminator(Block(b_u32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(cnt) = in_edges.get_mut(tb) {
                    *cnt += 1;
                }
                if let Some(sb) = succs.get_mut(b) {
                    if sb.scan_for(&edge.block).is_none() {
                        sb.push(edge.block);
                    }
                }
                if let Some(ptb) = preds.get_mut(tb) {
                    if ptb.scan_for(&b_u32).is_none() {
                        ptb.push(b_u32);
                    }
                }
            }
        });
    }

    // Step 1: Seed cold blocks (entry block `0` is never cold).
    let mut cold = filled(blocks, false, &alloc);
    for b in 1..blocks {
        let blk = Block(u32_of(b));
        let kind = view.block_kind(blk);
        if matches!(kind, BlockKind::Cold | BlockKind::Catch | BlockKind::Cleanup | BlockKind::Terminate) {
            set_at(&mut cold, b, true);
            continue;
        }
        let term = view.terminator(blk);
        if matches!(view.op(term), Op::Trap | Op::Unreachable | Op::Resume | Op::Cancelled) {
            set_at(&mut cold, b, true);
            continue;
        }
        for inst in view.block(blk) {
            let op = view.op(inst);
            match op {
                Op::Call | Op::Invoke | Op::TailCall => {
                    let ref_id = at(view.extra, to_idx(at(view.data, inst, 0)), 0);
                    if is_cold_callee(ref_id) || view.flags(inst).contains(Flags::COLD) {
                        set_at(&mut cold, b, true);
                        break;
                    }
                }
                Op::CallIndirect | Op::InvokeIndirect | Op::TailCallIndirect => {
                    if view.flags(inst).contains(Flags::COLD) {
                        set_at(&mut cold, b, true);
                        break;
                    }
                }
                _ => {}
            }
        }
        if at(&cold, b, false) {
            continue;
        }
        // Check unlikely branch hint (`Flags::EXPECT_FALSE` on `then`, `Flags::EXPECT_TRUE` on
        // `otherwise`) when this block has only a single incoming branch edge.
        if at(&in_edges, b, 0) == 1
            && let Some(pb) = preds.get(b)
            && pb.len() == 1
            && let Some(&pred0) = pb.first()
        {
            let pred_term = view.terminator(Block(pred0));
            if view.op(pred_term) == Op::Br {
                let flags = view.flags(pred_term);
                let mut o = view.operands(pred_term);
                let _cond = o.value();
                let then_b = to_idx(o.target().block.0);
                let else_b = to_idx(o.target().block.0);
                if then_b != else_b {
                    let expect_false = flags.contains(Flags::EXPECT_FALSE) && !flags.contains(Flags::EXPECT_TRUE);
                    let expect_true = flags.contains(Flags::EXPECT_TRUE) && !flags.contains(Flags::EXPECT_FALSE);
                    if (expect_false && then_b == b) || (expect_true && else_b == b) {
                        set_at(&mut cold, b, true);
                    }
                }
            }
        }
    }

    // Step 2: Fixed-point backward (all successors cold) and forward (all predecessors cold)
    // propagation. Bounded by `blocks + 1` iterations (deterministic).
    for _ in 0..=blocks {
        let mut changed = false;
        for b in 1..blocks {
            if at(&cold, b, false) {
                continue;
            }
            let all_succs_cold = succs
                .get(b)
                .is_some_and(|s| !s.is_empty() && s.iter().all(|&x| at(&cold, to_idx(x), false)));
            let all_preds_cold = preds
                .get(b)
                .is_some_and(|p| !p.is_empty() && p.iter().all(|&x| at(&cold, to_idx(x), false)));
            if all_succs_cold || all_preds_cold {
                set_at(&mut cold, b, true);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Step 3: Build partitioned layout order: all hot blocks in original relative order
    // (entry block `0` always first), followed by all cold blocks in original relative order.
    let mut order: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    let mut out_is_cold: Vec<bool, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        if !at(&cold, b, false) {
            order.push(b);
            out_is_cold.push(false);
        }
    }
    let mut cold_count = 0u32;
    for b in 0..blocks {
        if at(&cold, b, false) {
            order.push(b);
            out_is_cold.push(true);
            cold_count += 1;
        }
    }

    let mut new_block = filled(blocks, NONE, &alloc);
    let mut new_inst = filled(len, NONE, &alloc);
    let mut next_inst = 0u32;
    let mut blocks_sunk = 0u32;
    let mut any_change = false;
    for (new_b, &old_b) in order.iter().enumerate() {
        set_at(&mut new_block, old_b, u32_of(new_b));
        if new_b != old_b {
            any_change = true;
        }
        let old_cold = at(&cold, old_b, false);
        let old_blk = Block(u32_of(old_b));
        if old_cold && new_b > old_b {
            blocks_sunk += 1;
        }
        if old_cold && view.block_kind(old_blk) == BlockKind::Normal {
            any_change = true;
        }
        for inst in view.block(old_blk) {
            set_at(&mut new_inst, inst, next_inst);
            next_inst += 1;
        }
    }

    let stats = ColdSplitStats {
        cold_blocks_identified: cold_count,
        blocks_sunk,
    };

    if !any_change {
        return Ok(ColdSplitOutput {
            func: view.to_func_in(alloc),
            is_cold: out_is_cold,
            stats,
        });
    }

    // Step 4: Emit reordered blocks with remapped value and block indices.
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

    for &old_b in order.iter() {
        func.block_starts.push(u32_of(func.ops.len()));
        let old_blk = Block(u32_of(old_b));
        let old_kind = view.block_kind(old_blk);
        let kind = if at(&cold, old_b, false) && old_kind == BlockKind::Normal {
            BlockKind::Cold
        } else {
            old_kind
        };
        func.block_kinds.push(block_kind_byte(kind));
        for inst in view.block(old_blk) {
            copy_remapped(&view, inst, &new_inst, &new_block, &mut func)?;
        }
    }

    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(SimplifyError::Refused { inst: 0, what: "an output past LIR's columns" });
    }
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(ColdSplitOutput { func, is_cold: out_is_cold, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        IntCond,
        body::{Bodies, BodyId},
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Trap, Val},
        func::{Conv, RefId, RefKind},
    };

    struct PanicBody<'a> {
        panic_ref: RefId,
        panic_view: View<'a>,
    }

    impl Bodies for PanicBody<'_> {
        fn callee(&self, _from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            if reference == self.panic_ref {
                Some((BodyId([1; 32]), self.panic_view))
            } else {
                None
            }
        }
    }

    #[test]
    fn partition_cold_blocks_sinks_panic_and_trap_out_of_hot_loop() {
        // Callee `cold_panic(i64) -> ()` that returns cleanly so `cold_trap`'s `trap(1)` runs.
        let mut pb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let panic_sig = pb.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
        pb.set_signature(panic_sig);
        let p_entry = pb.create_block(BlockKind::Normal);
        pb.begin(p_entry);
        let _code = pb.param(Ty::I64);
        pb.ret(&[]);
        let panic_fn = pb.finish_checked().unwrap();

        // Hot loop with inline cold `if i == fail_at { call cold_panic(i); trap(1); }` blocks
        // laid out right in the middle of the loop:
        //   0: entry -> loop_hdr(0, 0)
        //   1: loop_hdr(i, acc) -> br (i == n) ? exit(acc) : body
        //   2: body -> br (i == fail_at) ? cold_panic(i) : latch
        //   3: cold_panic(bad_i) -> call cold_panic_ref(bad_i); jump cold_trap
        //   4: cold_trap -> trap(1)
        //   5: latch -> jump loop_hdr(i + 1, acc + i)
        //   6: exit(res) -> return res
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let call_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
        let cold_panic_ref = b.reference(RefKind::Func, 0);

        let [entry, loop_hdr, body, cold_panic, cold_trap, latch, exit] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let fail_at = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        b.jump((loop_hdr, &[zero, zero]));

        b.begin(loop_hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Eq, i, n);
        b.br(Flags::NONE, done, (exit, &[acc]), (body, &[]));

        b.begin(body);
        let is_err = b.icmp(IntCond::Eq, i, fail_at);
        b.br(Flags::NONE, is_err, (cold_panic, &[i]), (latch, &[]));

        b.begin(cold_panic);
        let bad_i = b.param(Ty::I64);
        let _ = b.call(Flags::NONE, cold_panic_ref, call_sig, &[bad_i]);
        b.jump((cold_trap, &[]));

        b.begin(cold_trap);
        b.trap(1);

        b.begin(latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((loop_hdr, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();

        let split = partition_cold_blocks(func.view(), |r| r == cold_panic_ref.0, TestHeap).unwrap();
        assert_eq!(split.stats.cold_blocks_identified, 2);
        assert_eq!(split.stats.blocks_sunk, 2);
        assert_eq!(split.is_cold.len(), 7);
        assert_eq!(&split.is_cold[..5], &[false, false, false, false, false]);
        assert_eq!(&split.is_cold[5..], &[true, true]);
        assert_eq!(split.func.view().block_kind(Block(5)), BlockKind::Cold);
        assert_eq!(split.func.view().block_kind(Block(6)), BlockKind::Cold);

        let bodies = PanicBody { panic_ref: cold_panic_ref, panic_view: panic_fn.view() };
        let root = BodyId([0; 32]);

        // Hot path: never triggers the cold error branch.
        for &(limit, fail) in &[(0u128, 99u128), (1, 99), (10, 99), (25, 100)] {
            let orig = eval::run_calls_in(func.view(), root, &[Val::Int(limit), Val::Int(fail)], 50_000, &bodies, TestHeap).unwrap();
            let part = eval::run_calls_in(split.func.view(), root, &[Val::Int(limit), Val::Int(fail)], 50_000, &bodies, TestHeap).unwrap();
            assert_eq!(orig, part, "hot path mismatch at limit={limit}");
            let expected = limit * limit.saturating_sub(1) / 2;
            assert!(matches!(part, Outcome::Returned(ref v) if v.as_slice() == [Val::Int(expected)]));
        }

        // Cold path: hits `cold_panic` -> `cold_trap` -> `Trap::Code(1)`.
        for &(limit, fail) in &[(10u128, 0u128), (10, 4), (10, 9)] {
            let orig = eval::run_calls_in(func.view(), root, &[Val::Int(limit), Val::Int(fail)], 50_000, &bodies, TestHeap).unwrap();
            let part = eval::run_calls_in(split.func.view(), root, &[Val::Int(limit), Val::Int(fail)], 50_000, &bodies, TestHeap).unwrap();
            assert_eq!(orig, part, "cold path mismatch at fail={fail}");
            assert!(matches!(part, Outcome::Trapped(Trap::Code(1))));
        }
    }

    #[test]
    fn partition_cold_blocks_propagates_expect_false_and_invoke_cleanup() {
        // Exercises:
        // - `Flags::EXPECT_FALSE` on a `Br` (sole-pred `then` block becomes cold + forward-propagates)
        // - `Op::Invoke` with `BlockKind::Cleanup` landing pad remapped cleanly
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        let unlikely_head = b.create_block(BlockKind::Normal);
        let unlikely_tail = b.create_block(BlockKind::Normal);
        let cleanup = b.create_block(BlockKind::Cleanup);
        let hot_invoke = b.create_block(BlockKind::Normal);
        let hot_done = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let cond = b.param(Ty::I1);
        b.br(Flags::EXPECT_FALSE, cond, (unlikely_head, &[]), (hot_invoke, &[]));

        b.begin(unlikely_head);
        b.jump((unlikely_tail, &[]));

        b.begin(unlikely_tail);
        let k99 = b.iconst(Ty::I64, 99);
        b.ret(&[k99]);

        b.begin(cleanup);
        let exc = b.param(Ty::PTR);
        b.resume(exc);

        b.begin(hot_invoke);
        let rv = b.invoke(Flags::NONE, callee_ref, callee_sig, &[], hot_done, cleanup);

        b.begin(hot_done);
        b.ret(&[rv]);

        let func = b.finish_checked().unwrap();

        let split = partition_cold_blocks(func.view(), |_| false, TestHeap).unwrap();
        // `unlikely_head`, `unlikely_tail` (forward-propagated), and `cleanup` are all cold and sunk!
        assert_eq!(split.stats.cold_blocks_identified, 3);
        assert_eq!(split.stats.blocks_sunk, 3);
        assert_eq!(&split.is_cold[..3], &[false, false, false]);
        assert_eq!(&split.is_cold[3..], &[true, true, true]);
        assert_eq!(split.func.view().block_kind(Block(5)), BlockKind::Cleanup);
    }

    #[test]
    fn partition_cold_blocks_backward_propagates_from_trap_and_unreachable() {
        // `err_dispatch` has no `Trap`/`Unreachable` or cold callee itself, but BOTH of its
        // successors (`trap_a` and `unreach_b`) are cold, so backward propagation marks it cold
        // and sinks all three past `hot_ret`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I1), Param::of(Ty::I1)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let [entry, err_dispatch, trap_a, unreach_b, hot_ret] =
            [(); 5].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let is_err = b.param(Ty::I1);
        let kind = b.param(Ty::I1);
        b.br(Flags::NONE, is_err, (err_dispatch, &[]), (hot_ret, &[]));

        b.begin(err_dispatch);
        b.br(Flags::NONE, kind, (trap_a, &[]), (unreach_b, &[]));

        b.begin(trap_a);
        b.trap(7);

        b.begin(unreach_b);
        b.unreachable();

        b.begin(hot_ret);
        let ok = b.iconst(Ty::I64, 42);
        b.ret(&[ok]);

        let func = b.finish_checked().unwrap();
        let split = partition_cold_blocks(func.view(), |_| false, TestHeap).unwrap();

        assert_eq!(split.stats.cold_blocks_identified, 3);
        assert_eq!(split.stats.blocks_sunk, 3);
        assert_eq!(&split.is_cold[..2], &[false, false]);
        assert_eq!(&split.is_cold[2..], &[true, true, true]);

        let orig = eval::run_in(func.view(), &[Val::Int(0), Val::Int(0)], 10_000, TestHeap).unwrap();
        let part = eval::run_in(split.func.view(), &[Val::Int(0), Val::Int(0)], 10_000, TestHeap).unwrap();
        assert_eq!(orig, part);
        assert!(matches!(part, Outcome::Returned(ref v) if v.as_slice() == [Val::Int(42)]));

        let orig_t = eval::run_in(func.view(), &[Val::Int(1), Val::Int(1)], 10_000, TestHeap).unwrap();
        let part_t = eval::run_in(split.func.view(), &[Val::Int(1), Val::Int(1)], 10_000, TestHeap).unwrap();
        assert_eq!(orig_t, part_t);
        assert!(matches!(part_t, Outcome::Trapped(Trap::Code(7))));
    }
}

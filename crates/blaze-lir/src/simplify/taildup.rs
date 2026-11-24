//! Interpreter / State-Machine `Switch` Tail Duplication (`blaze_lir::taildup`).
//!
//! Bytecode VMs (such as `revm` in `reth`), parsers, and state machines spend most of their time
//! jumping from each opcode arm back to a single shared `Op::Switch` dispatch block. A single
//! shared indirect jump (`br x17` / `jmp rax`) aliases every opcode transition in the CPU's
//! indirect branch target buffer (BTB), causing heavy mispredictions.
//!
//! This pass detects dense `Op::Switch` dispatch blocks (`>= 4` cases, small read-only dispatch
//! header) and duplicates the dispatch fetch + `Op::Switch` directly into the tail of each
//! compact arm ending in `Jump(dispatch, args)` — giving the hardware branch predictor **per-opcode
//! indirect transition history** (the same optimization as computed-goto threaded dispatch, with
//! zero source changes)!
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, BlockKind, Func, Op, Ty, View,
    op::{Field, Layout},
    verify,
};

/// Telemetry returned by [`tail_duplicate_dispatch`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct TailDupStats {
    pub dispatches_found: u32,
    pub arms_duplicated: u32,
}

/// Output of [`tail_duplicate_dispatch`].
#[derive(Debug)]
pub struct TailDupOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: TailDupStats,
}

const MIN_SWITCH_CASES: usize = 4;
const MAX_DISPATCH_INSTS: usize = 12;
const MAX_ARM_INSTS: usize = 24;
const MAX_DUPLICATED_ARMS: usize = 16;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
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
fn is_duplicable_dispatch_op(op: Op) -> bool {
    !matches!(
        op,
        Op::Store
            | Op::Call
            | Op::CallIndirect
            | Op::Invoke
            | Op::InvokeIndirect
            | Op::AtomicStore
            | Op::AtomicRmw
            | Op::Cmpxchg
            | Op::Fence
            | Op::Memcpy
            | Op::Memmove
            | Op::Memset
            | Op::Asm
            | Op::AsmBr
    )
}

fn copy_remapped<A: Allocator>(view: &View<'_>, inst: usize, val_map: &[u32], func: &mut Func<A>) -> u32 {
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
                        func.extra.push(at(view.extra, p));
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
                            func.extra.push(at(view.extra, p));
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

/// Emits `Op::Switch` at `inst`, appending promoted `disp` defs onto any target block's edge args.
fn copy_switch_with_promoted<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    promoted: &[Vec<u32, A>],
    func: &mut Func<A>,
) -> u32 {
    let src = to_idx(at(view.data, inst));
    let dst = to_u32(func.extra.len());
    let scrutinee = at(view.extra, src);
    let w = to_idx(Ty(at(view.tys, to_idx(scrutinee))).const_words());
    func.extra.push(at(val_map, to_idx(scrutinee)));
    let mut p = src + 1;
    // Default target:
    let def_b = at(view.extra, p);
    let def_an = to_idx(at(view.extra, p + 1));
    let def_extra = promoted.get(to_idx(def_b)).map(Vec::as_slice).unwrap_or(&[]);
    func.extra.push(def_b);
    func.extra.push(to_u32(def_an + def_extra.len()));
    p += 2;
    for k in 0..def_an {
        func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
    }
    for &pv in def_extra.iter() {
        func.extra.push(at(val_map, to_idx(pv)));
    }
    p += def_an;
    // Cases:
    let n_cases_u32 = at(view.extra, p);
    let n_cases = to_idx(n_cases_u32);
    func.extra.push(n_cases_u32);
    p += 1;
    for _ in 0..n_cases {
        for k in 0..w {
            func.extra.push(at(view.extra, p + k));
        }
        p += w;
        let cb = at(view.extra, p);
        let can = to_idx(at(view.extra, p + 1));
        let c_extra = promoted.get(to_idx(cb)).map(Vec::as_slice).unwrap_or(&[]);
        func.extra.push(cb);
        func.extra.push(to_u32(can + c_extra.len()));
        p += 2;
        for k in 0..can {
            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
        }
        for &pv in c_extra.iter() {
            func.extra.push(at(val_map, to_idx(pv)));
        }
        p += can;
    }
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(Op::Switch));
    func.flags.push(at(view.flags, inst));
    func.tys.push(at(view.tys, inst));
    func.data.push(dst);
    func.attrs.push(at(view.attrs, inst));
    func.locs.push(at(view.locs, inst));
    idx
}

/// Duplicates compact `Op::Switch` interpreter dispatch headers directly into the tails of hot arms
/// that jump back to them, enabling per-opcode indirect branch prediction.
pub fn tail_duplicate_dispatch<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<TailDupOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 5 {
        return Ok(TailDupOutput { func: view.to_func_in(alloc), stats: TailDupStats::default() });
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

    // Find an eligible `Op::Switch` dispatch block `disp` and collect any `disp` defs directly used
    // in single-predecessor switch targets (which will be promoted back to block params on those targets).
    let mut target_disp: Option<(usize, Vec<Vec<u32, A>, A>)> = None;
    for b in 1..blocks {
        let b_u32 = to_u32(b);
        if view.block_kind(Block(b_u32)) != BlockKind::Normal {
            continue;
        }
        let range = view.block(Block(b_u32));
        let params = view.params(Block(b_u32));
        let term = range.end - 1;
        if view.op(term) != Op::Switch {
            continue;
        }
        let body_len = term - params.end;
        if body_len > MAX_DISPATCH_INSTS {
            continue;
        }
        let sw_at = to_idx(at(view.data, term));
        let def_argc = to_idx(at(view.extra, sw_at + 2));
        let n_cases = to_idx(at(view.extra, sw_at + 3 + def_argc));
        if n_cases < MIN_SWITCH_CASES {
            continue;
        }
        if !(params.end..term).all(|i| is_duplicable_dispatch_op(view.op(i))) {
            continue;
        }

        let mut is_sw_target = filled(blocks, false, &alloc);
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                set_at(&mut is_sw_target, to_idx(edge.block), true);
            }
        });

        let mut promoted: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
        for _ in 0..blocks {
            promoted.push(Vec::new_in(alloc));
        }
        let mut escapes = false;
        for ob in 0..blocks {
            if ob == b {
                continue;
            }
            for inst in view.block(Block(to_u32(ob))) {
                let mut check_val = |v: u32| {
                    if at(&inst_block, to_idx(v)) == b_u32 {
                        if at(&is_sw_target, ob)
                            && preds.get(ob).is_some_and(|p| p.len() == 1 && p.first().copied() == Some(b_u32))
                        {
                            if let Some(prom) = promoted.get_mut(ob)
                                && prom.scan_for(&v).is_none()
                            {
                                prom.push(v);
                            }
                        } else {
                            escapes = true;
                        }
                    }
                };
                each(&view, inst, |item| match item {
                    Item::Value(v) => check_val(v),
                    Item::Edge(edge) => {
                        if let Some((start, count)) = edge.args {
                            for k in 0..count {
                                check_val(at(view.extra, start + k));
                            }
                        }
                    }
                });
                if escapes {
                    break;
                }
            }
            if escapes {
                break;
            }
        }
        if escapes {
            continue;
        }
        // Ensure all outside values read by `disp` (including switch edge args!) come from entry block `0`.
        let mut uses_ok = true;
        for inst in params.end..range.end {
            let mut check_src = |v: u32| {
                let vb = to_idx(at(&inst_block, to_idx(v)));
                if vb != b && vb != 0 {
                    uses_ok = false;
                }
            };
            each(&view, inst, |item| match item {
                Item::Value(v) => check_src(v),
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            check_src(at(view.extra, start + k));
                        }
                    }
                }
            });
            if !uses_ok {
                break;
            }
        }
        if !uses_ok {
            continue;
        }
        target_disp = Some((b, promoted));
        break;
    }

    let Some((disp, promoted)) = target_disp else {
        return Ok(TailDupOutput { func: view.to_func_in(alloc), stats: TailDupStats::default() });
    };

    // Identify compact back-edge arms `arm > disp` ending in `Jump(disp, args)`!
    let mut dup_arm = filled(blocks, false, &alloc);
    let mut dup_count = 0usize;
    let disp_u32 = to_u32(disp);
    let disp_params = view.params(Block(disp_u32));
    for b in (disp + 1)..blocks {
        if dup_count >= MAX_DUPLICATED_ARMS {
            break;
        }
        let b_u32 = to_u32(b);
        if view.block_kind(Block(b_u32)) != BlockKind::Normal {
            continue;
        }
        let range = view.block(Block(b_u32));
        let term = range.end - 1;
        if view.op(term) != Op::Jump {
            continue;
        }
        if term - range.start > MAX_ARM_INSTS {
            continue;
        }
        let jat = to_idx(at(view.data, term));
        if to_idx(at(view.extra, jat)) == disp && to_idx(at(view.extra, jat + 1)) == disp_params.len() {
            set_at(&mut dup_arm, b, true);
            dup_count += 1;
        }
    }

    if dup_count < 2 {
        return Ok(TailDupOutput { func: view.to_func_in(alloc), stats: TailDupStats::default() });
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
    let mut block_map = filled(len, NONE, &alloc);
    let mut local_map = filled(len, NONE, &alloc);
    let disp_range = view.block(Block(disp_u32));
    let disp_term = disp_range.end - 1;

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let params_end = view.params(Block(b_u32)).end;

        if b == disp {
            for inst in range.start..disp_term {
                let mapped = copy_remapped(&view, inst, &val_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let mapped = copy_switch_with_promoted(&view, disp_term, &val_map, &promoted, &mut func);
            set_at(&mut val_map, disp_term, mapped);
            continue;
        }

        // Copy `val_map` into `block_map`, materializing any promoted `Op::Param`s for `b`!
        for (dst, &src) in block_map.iter_mut().zip(val_map.iter()) {
            *dst = src;
        }
        for inst in range.start..params_end {
            let mapped = copy_remapped(&view, inst, &block_map, &mut func);
            set_at(&mut val_map, inst, mapped);
            set_at(&mut block_map, inst, mapped);
        }
        if let Some(prom) = promoted.get(b) {
            for &pv in prom.iter() {
                let new_p = to_u32(func.ops.len());
                func.ops.push(op_byte(Op::Param));
                func.flags.push(0);
                func.tys.push(at(view.tys, to_idx(pv)));
                func.data.push(0);
                func.attrs.push(0);
                func.locs.push(0);
                set_at(&mut block_map, to_idx(pv), new_p);
            }
        }

        if at(&dup_arm, b) {
            let term = range.end - 1;
            for inst in params_end..term {
                let mapped = copy_remapped(&view, inst, &block_map, &mut func);
                set_at(&mut val_map, inst, mapped);
                set_at(&mut block_map, inst, mapped);
            }
            // Prepare `local_map`: all entry-block (0) defs + `disp`'s params mapped to `Jump` args!
            for (dst, &src) in local_map.iter_mut().zip(block_map.iter()) {
                *dst = src;
            }
            let jat = to_idx(at(view.data, term));
            for k in 0..disp_params.len() {
                let arg = at(&block_map, to_idx(at(view.extra, jat + 2 + k)));
                set_at(&mut local_map, disp_params.start + k, arg);
            }
            // Splice `disp`'s body + promoted `Op::Switch` directly into the tail of `arm`!
            for d_inst in disp_params.end..disp_term {
                let mapped = copy_remapped(&view, d_inst, &local_map, &mut func);
                set_at(&mut local_map, d_inst, mapped);
            }
            let mapped = copy_switch_with_promoted(&view, disp_term, &local_map, &promoted, &mut func);
            set_at(&mut local_map, disp_term, mapped);
        } else {
            for inst in params_end..range.end {
                let mapped = copy_remapped(&view, inst, &block_map, &mut func);
                set_at(&mut val_map, inst, mapped);
                set_at(&mut block_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(TailDupOutput {
        func,
        stats: TailDupStats {
            dispatches_found: 1,
            arms_duplicated: to_u32(dup_count),
        },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        Flags,
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::Conv,
    };

    #[test]
    fn tail_duplicate_dispatch_duplicates_switch_into_vm_arms_and_matches_eval() {
        // Mini 4-opcode state machine (opcode = state & 3):
        //   0: acc += 3; state = 1
        //   1: acc ^= 7; state = 2
        //   2: acc += 11; state = 3
        //   3: acc += 1; state = 0 (until steps == 0 -> halt!)
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, disp, op0, op1, op2, op3, halt] = [(); 7].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let steps = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let k1 = b.iconst(Ty::I64, 1);
        let k3 = b.iconst(Ty::I64, 3);
        let k4 = b.iconst(Ty::I64, 4);
        let k7 = b.iconst(Ty::I64, 7);
        let k11 = b.iconst(Ty::I64, 11);
        b.jump((disp, &[steps, z, z]));

        b.begin(disp);
        let rem = b.param(Ty::I64);
        let st = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let is_z = b.icmp(crate::IntCond::Eq, rem, z);
        let code = b.select(Flags::NONE, is_z, k4, st);
        b.switch(
            code,
            (halt, &[acc]),
            &[
                (0, (op0, &[rem, acc])),
                (1, (op1, &[rem, acc])),
                (2, (op2, &[rem, acc])),
                (3, (op3, &[rem, acc])),
            ],
        );

        let arms = [(op0, Op::Add, k3, 1u128), (op1, Op::Xor, k7, 2), (op2, Op::Add, k11, 3), (op3, Op::Add, k1, 0)];
        for (blk, alu, kv, next_st) in arms {
            b.begin(blk);
            let r = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let r2 = b.binary(Op::Sub, Flags::NONE, r, k1);
            let a2 = b.binary(alu, Flags::NONE, a, kv);
            let ns = b.iconst(Ty::I64, next_st);
            b.jump((disp, &[r2, ns, a2]));
        }
        b.begin(halt);
        let out = b.param(Ty::I64);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let dup = tail_duplicate_dispatch(func.view(), TestHeap).unwrap();
        assert_eq!(dup.stats.dispatches_found, 1);
        assert_eq!(dup.stats.arms_duplicated, 4);
        let sw_count = (0..dup.func.view().len()).filter(|&i| dup.func.view().op(i) == Op::Switch).count();
        assert_eq!(sw_count, 5, "expected 1 initial + 4 per-arm Switch dispatches!");

        for &n in &[0u128, 1, 2, 3, 4, 15, 40] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(dup.func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "taildup mismatch at n={n}");
        }
    }
}

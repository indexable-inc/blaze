//! PGO-Guided Speculative Devirtualization for `Op::CallIndirect` (`dyn Trait` & fn pointers).
//!
//! Static compilers (including LLVM `-O3` without LTO/ICP profiles) cannot inline through
//! indirect function pointers or trait-object vtables. With Blaze's runtime PGO target profiles
//! (`BPG1`), a monomorphic or dominant indirect call site (`>= min_hit_percent`, default 80%) is
//! rewritten into a branch-predicted guarded direct call:
//!
//! ```text
//!   expected_fp = func_addr(hot_callee)
//!   if ptr_addr(fp) == ptr_addr(expected_fp) {
//!       res = call hot_callee(args...)         // DIRECT -> inlinable, SROA-able, vectorizable!
//!   } else {
//!       res = call_indirect [cold] fp(args...) // COLD -> sunk out of hot loop!
//!   }
//! ```
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{SimplifyError, dce::{NONE, filled}};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    func::{BlockKind, RefId, SigId},
    verify,
};

/// One PGO indirect-call target observation for a `CallIndirect` site (`dyn Trait` or fn pointer).
///
/// Supports both **monomorphic** (`secondary: None`) and **bimorphic** (`secondary: Some(...)`)
/// inline caches: both hot targets become direct `Op::Call` sites eligible for immediate inlining!
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndirectTarget {
    /// Instruction index of the `Op::CallIndirect` in the input function.
    pub call_inst: u32,
    /// Primary observed hot callee reference (must exist in `func.refs` with `RefKind::Func`).
    pub callee: RefId,
    /// Expected signature matching the `CallIndirect` site.
    pub sig: SigId,
    /// Percentage (`0..=100`) of dynamic executions that targeted `callee`.
    pub hit_percent: u8,
    /// Optional second hot callee `(callee2, hit_percent2)` for bimorphic `dyn Trait` call sites.
    pub secondary: Option<(RefId, u8)>,
}

impl IndirectTarget {
    /// Constructs a monomorphic indirect-call target observation.
    pub const fn monomorphic(call_inst: u32, callee: RefId, sig: SigId, hit_percent: u8) -> Self {
        Self { call_inst, callee, sig, hit_percent, secondary: None }
    }

    /// Constructs a bimorphic (`2-target`) indirect-call target observation for `dyn Trait` sites.
    pub const fn bimorphic(
        call_inst: u32,
        callee1: RefId,
        hit1: u8,
        callee2: RefId,
        hit2: u8,
        sig: SigId,
    ) -> Self {
        Self { call_inst, callee: callee1, sig, hit_percent: hit1, secondary: Some((callee2, hit2)) }
    }

    /// Total percentage covered by the guarded direct target(s).
    #[inline]
    pub const fn total_hit_percent(&self) -> u8 {
        match self.secondary {
            Some((_, h2)) => self.hit_percent.saturating_add(h2),
            None => self.hit_percent,
        }
    }
}

/// Telemetry returned by [`speculative_devirt`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct DevirtStats {
    pub sites_devirtualized: u32,
}

/// Output of [`speculative_devirt`].
#[derive(Debug)]
pub struct DevirtOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: DevirtStats,
}

/// Rewrites the first eligible hot `Op::CallIndirect` site (`hit_percent >= min_hit_percent`) into
/// a branch-predicted guarded direct `Op::Call` + cold `Op::CallIndirect` fallback.
///
/// Call in a loop or before `inline_guided_with_cache` / `lto_optimize_module` so every hot
/// devirtualized site immediately unlocks cross-function inlining and post-inline SROA/SIMD!
pub fn speculative_devirt<A: Allocator + Copy>(
    view: View<'_>,
    targets: &[IndirectTarget],
    min_hit_percent: u8,
    alloc: A,
) -> Result<DevirtOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut total = 0u32;

    // Sort targets descending by `call_inst` so splitting blocks at higher instruction indices
    // never shifts instruction indices of earlier sites!
    let mut sorted: Vec<IndirectTarget, A> = Vec::with_capacity_in(targets.len(), alloc);
    for &t in targets {
        if t.total_hit_percent() >= min_hit_percent {
            sorted.push(t);
        }
    }
    sorted.sort_unstable_by(|a, b| b.call_inst.cmp(&a.call_inst));

    for target in sorted {
        let (next, did) = devirt_one(cur.view(), target, alloc)?;
        if did {
            cur = next;
            total += 1;
        }
    }

    Ok(DevirtOutput {
        func: cur,
        stats: DevirtStats { sites_devirtualized: total },
    })
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
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
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
fn int_cond_word(c: IntCond) -> u32 {
    IntCond::ALL.iter().position(|&x| x == c).and_then(|i| u32::try_from(i).ok()).unwrap_or(0)
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

fn devirt_one<A: Allocator + Copy>(
    view: View<'_>,
    target: IndirectTarget,
    alloc: A,
) -> Result<(Func<A>, bool), SimplifyError> {
    let ci = to_idx(target.call_inst);
    if ci >= view.len() || view.op(ci) != Op::CallIndirect {
        return Ok((view.to_func_in(alloc), false));
    }
    if target.callee.index() >= view.refs.len() {
        return Ok((view.to_func_in(alloc), false));
    }
    let extra_at = to_idx(at(view.data, ci, 0));
    let fp_val = at(view.extra, extra_at, 0);
    let sig_id = SigId(at(view.extra, extra_at + 1, 0));
    if sig_id != target.sig {
        return Ok((view.to_func_in(alloc), false));
    }
    let argc_u32 = at(view.extra, extra_at + 2, 0);
    let argc = to_idx(argc_u32);
    let ret_ty = Ty(at(view.tys, ci, 0));

    // Locate the block containing `ci`.
    let blocks = view.blocks();
    let mut call_blk = None;
    for b in 0..blocks {
        let r = view.block(Block(u32_of(b)));
        if ci >= r.start && ci < r.end {
            call_blk = Some(b);
            break;
        }
    }
    let Some(split_b) = call_blk else {
        return Ok((view.to_func_in(alloc), false));
    };

    // Split `split_b` into `[split_b (guard), fast_b (direct Call), slow_b (cold CallIndirect), merge_b (tail)]`.
    // Blocks `<= split_b` keep their index; blocks `> split_b` shift by `+3`.
    // Wait: back-edges targeting `split_b` still target `split_b` (its entry!), while edges leaving `split_b`
    // move into `merge_b` — so EVERY existing predecessor edge targeting `split_b` stays 100% valid!
    let secondary = target.secondary.filter(|&(r, h)| h > 0 && r.index() < view.refs.len() && r != target.callee);
    let extra_blks: usize = if secondary.is_some() { 5 } else { 3 };
    let fast1_blk = u32_of(split_b + 1);
    let check2_blk = u32_of(split_b + 2);
    let fast2_blk = u32_of(split_b + 3);
    let slow_blk = if secondary.is_some() { u32_of(split_b + 4) } else { u32_of(split_b + 2) };
    let merge_blk = u32_of(split_b + extra_blks);

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b <= split_b { u32_of(b) } else { u32_of(b + extra_blks) });
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

    let mut val_map = filled(view.len(), NONE, &alloc);

    let push = |f: &mut Func<A>, op: Op, fl: Flags, ty: Ty, data: u32, loc: u32| -> u32 {
        let idx = u32_of(f.ops.len());
        f.ops.push(op_byte(op));
        f.flags.push(fl.0);
        f.tys.push(ty.0);
        f.data.push(data);
        f.attrs.push(0);
        f.locs.push(loc);
        idx
    };

    for b in 0..blocks {
        func.block_starts.push(u32_of(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b, 0));
        let range = view.block(Block(u32_of(b)));

        if b == split_b {
            // Emit instructions strictly before `ci`.
            for inst in range.start..ci {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let loc = at(view.locs, ci, 0);
            let fp_new = at(&val_map, to_idx(fp_val), NONE);
            let mut args_new: Vec<u32, A> = Vec::with_capacity_in(argc, alloc);
            for k in 0..argc {
                args_new.push(at(&val_map, to_idx(at(view.extra, extra_at + 3 + k, 0)), NONE));
            }

            // Guard 1: `ptr_addr(fp) == ptr_addr(func_addr(callee1))`.
            let expected_fp = push(&mut func, Op::FuncAddr, Flags::NONE, Ty::PTR, target.callee.0, loc);
            let fp_int = push(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, fp_new, loc);
            let exp_int = push(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, expected_fp, loc);
            let cmp_at = u32_of(func.extra.len());
            func.extra.push(int_cond_word(IntCond::Eq));
            func.extra.push(fp_int);
            func.extra.push(exp_int);
            let is_hot = push(&mut func, Op::Icmp, Flags::NONE, Ty::I1, cmp_at, loc);
            let br_at = u32_of(func.extra.len());
            func.extra.push(is_hot);
            func.extra.push(fast1_blk);
            func.extra.push(0);
            func.extra.push(if secondary.is_some() { check2_blk } else { slow_blk });
            func.extra.push(0);
            push(&mut func, Op::Br, Flags::EXPECT_TRUE, Ty::NONE, br_at, loc);

            // Fast block 1: direct `Op::Call(callee1, sig, args...)` -> ready for immediate inlining!
            func.block_starts.push(u32_of(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let fast_call_at = u32_of(func.extra.len());
            func.extra.push(target.callee.0);
            func.extra.push(sig_id.0);
            func.extra.push(argc_u32);
            for &a in &args_new {
                func.extra.push(a);
            }
            let fast_res = push(&mut func, Op::Call, Flags::NONE, ret_ty, fast_call_at, loc);
            let fj_at = u32_of(func.extra.len());
            func.extra.push(merge_blk);
            if ret_ty != Ty::NONE {
                func.extra.push(1);
                func.extra.push(fast_res);
            } else {
                func.extra.push(0);
            }
            push(&mut func, Op::Jump, Flags::NONE, Ty::NONE, fj_at, loc);

            // Optional bimorphic second target (`dyn Trait` 2-target inline cache)!
            if let Some((callee2, _)) = secondary {
                func.block_starts.push(u32_of(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let exp2 = push(&mut func, Op::FuncAddr, Flags::NONE, Ty::PTR, callee2.0, loc);
                let exp2_int = push(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, exp2, loc);
                let cmp2_at = u32_of(func.extra.len());
                func.extra.push(int_cond_word(IntCond::Eq));
                func.extra.push(fp_int);
                func.extra.push(exp2_int);
                let is_hot2 = push(&mut func, Op::Icmp, Flags::NONE, Ty::I1, cmp2_at, loc);
                let br2_at = u32_of(func.extra.len());
                func.extra.push(is_hot2);
                func.extra.push(fast2_blk);
                func.extra.push(0);
                func.extra.push(slow_blk);
                func.extra.push(0);
                push(&mut func, Op::Br, Flags::EXPECT_TRUE, Ty::NONE, br2_at, loc);

                func.block_starts.push(u32_of(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let fast2_call_at = u32_of(func.extra.len());
                func.extra.push(callee2.0);
                func.extra.push(sig_id.0);
                func.extra.push(argc_u32);
                for &a in &args_new {
                    func.extra.push(a);
                }
                let fast2_res = push(&mut func, Op::Call, Flags::NONE, ret_ty, fast2_call_at, loc);
                let fj2_at = u32_of(func.extra.len());
                func.extra.push(merge_blk);
                if ret_ty != Ty::NONE {
                    func.extra.push(1);
                    func.extra.push(fast2_res);
                } else {
                    func.extra.push(0);
                }
                push(&mut func, Op::Jump, Flags::NONE, Ty::NONE, fj2_at, loc);
            }

            // Slow block: cold `Op::CallIndirect` fallback (automatically sunk out of hot loops!).
            func.block_starts.push(u32_of(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let slow_call_at = u32_of(func.extra.len());
            func.extra.push(fp_new);
            func.extra.push(sig_id.0);
            func.extra.push(argc_u32);
            for &a in &args_new {
                func.extra.push(a);
            }
            let slow_res = push(&mut func, Op::CallIndirect, Flags::COLD, ret_ty, slow_call_at, loc);
            let sj_at = u32_of(func.extra.len());
            func.extra.push(merge_blk);
            if ret_ty != Ty::NONE {
                func.extra.push(1);
                func.extra.push(slow_res);
            } else {
                func.extra.push(0);
            }
            push(&mut func, Op::Jump, Flags::NONE, Ty::NONE, sj_at, loc);

            // Merge block: receives `call` result via block param and runs remaining instructions of `split_b`.
            func.block_starts.push(u32_of(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            if ret_ty != Ty::NONE {
                let merged = push(&mut func, Op::Param, Flags::NONE, ret_ty, 0, loc);
                set_at(&mut val_map, ci, merged);
            }
            for inst in (ci + 1)..range.end {
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
    Ok((func, true))
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    use crate::op::{Field, Layout};
    let op = view.op(inst);
    let flags = at(view.flags, inst, 0);
    let ty = at(view.tys, inst, 0);
    let attr = at(view.attrs, inst, 0);
    let loc = at(view.locs, inst, 0);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst, 0)), NONE),
        Layout::Data(Field::Block) => at(blk_map, to_idx(at(view.data, inst, 0)), NONE),
        Layout::Data(_) => at(view.data, inst, 0),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst, 0));
            let dst = u32_of(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at(val_map, to_idx(at(view.extra, p, 0)), NONE));
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(at(blk_map, to_idx(at(view.extra, p, 0)), NONE));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k, 0)), NONE));
                        }
                        p += n;
                    }
                    Field::Target => {
                        let b = to_idx(at(view.extra, p, 0));
                        let n_u32 = at(view.extra, p + 1, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(at(blk_map, b, NONE));
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k, 0)), NONE));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at(view.extra, src, 0);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee), 0)).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k, 0));
                            }
                            p += w;
                            let b = to_idx(at(view.extra, p, 0));
                            let an_u32 = at(view.extra, p + 1, 0);
                            let an = to_idx(an_u32);
                            func.extra.push(at(blk_map, b, NONE));
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k, 0)), NONE));
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
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(blk_map, to_idx(at(view.extra, p + k, 0)), NONE));
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
                    _ => {
                        func.extra.push(at(view.extra, p, 0));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = u32_of(func.ops.len());
    func.ops.push(at(view.ops, inst, 0));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        func::{Conv, RefKind},
    };

    #[test]
    fn speculative_devirt_splits_hot_indirect_call_into_guarded_direct_call() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let cb_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let fn_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(fn_sig);
        let hot_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let fp = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let res = b.call_indirect(Flags::NONE, fp, cb_sig, &[x]);
        let k1 = b.iconst(Ty::I64, 1);
        let out = b.binary(Op::Add, Flags::NONE, res, k1);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let call_inst = (0..func.view().len()).find(|&i| func.view().op(i) == Op::CallIndirect).unwrap() as u32;
        let targets = [IndirectTarget::monomorphic(call_inst, hot_ref, cb_sig, 98)];
        let dev = speculative_devirt(func.view(), &targets, 80, TestHeap).unwrap();
        assert_eq!(dev.stats.sites_devirtualized, 1);
        let v = dev.func.view();
        let has_direct = (0..v.len()).any(|i| v.op(i) == Op::Call);
        let has_cold_indirect = (0..v.len()).any(|i| v.op(i) == Op::CallIndirect && Flags(v.flags[i]).contains(Flags::COLD));
        assert!(has_direct && has_cold_indirect, "expected guarded direct Call + cold CallIndirect fallback");

        // Also test bimorphic (2-target) `dyn Trait` inline cache!
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let cb2 = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let fn2 = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b2.set_signature(fn2);
        let r1 = b2.reference(RefKind::Func, 0);
        let r2 = b2.reference(RefKind::Func, 1);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let fp2 = b2.param(Ty::PTR);
        let x2 = b2.param(Ty::I64);
        let res2 = b2.call_indirect(Flags::NONE, fp2, cb2, &[x2]);
        b2.ret(&[res2]);
        let f2 = b2.finish_checked().unwrap();
        let ci2 = (0..f2.view().len()).find(|&i| f2.view().op(i) == Op::CallIndirect).unwrap() as u32;
        let bi = speculative_devirt(f2.view(), &[IndirectTarget::bimorphic(ci2, r1, 55, r2, 43, cb2)], 80, TestHeap).unwrap();
        assert_eq!(bi.stats.sites_devirtualized, 1);
        let direct_calls = (0..bi.func.view().len()).filter(|&i| bi.func.view().op(i) == Op::Call).count();
        assert_eq!(direct_calls, 2, "bimorphic dyn devirt must emit 2 direct Call arms");
    }
}

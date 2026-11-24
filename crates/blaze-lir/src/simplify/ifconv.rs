//! If-Conversion (`Br` -> `Select`) + Conditional Reduction Canonicalization (`blaze_lir::ifconv`).
//!
//! Converts pure, side-effect-free diamond and triangle branches into branchless `Op::Select`
//! expressions and folds the resulting single-predecessor `Jump` via `coalesce_cfg`:
//!
//! - Eliminates unpredictable branches inside hot loops (turning ~15-cycle branch mispredicts
//!   into 1-cycle `csel`/`cmovcc` or SIMD `cmhi`+`and`/`bsl`).
//! - Canonicalizes conditional reductions:
//!   `Select(c, acc + x, acc)  ->  acc + Select(c, x, 0)` (and likewise for `Xor`, `Or`, `And`),
//!   exposing filtered Rust loops (`for &x in s { if x > t { acc += x; } }`) directly to
//!   4x-unrolled masked SIMD auto-vectorization!
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, coalesce_cfg, dead, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, Op, Ty, View,
    op::{Field, Layout},
    verify,
};

/// Telemetry returned by [`if_convert`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct IfConvStats {
    pub branches_converted: u32,
    pub reductions_canonicalized: u32,
}

/// Output of [`if_convert`].
#[derive(Debug)]
pub struct IfConvOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: IfConvStats,
}

const MAX_ARM_INSTS: usize = 8;

#[inline]
fn is_speculable_pure(op: Op, flags: Flags) -> bool {
    if op.info().flags.meanings_only(flags).0 != 0 {
        return false;
    }
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
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
            | Op::Fneg
            | Op::Fabs
            | Op::PtrAdd
    )
}

#[inline]
fn is_selectable_ty(ty: Ty) -> bool {
    ty != Ty::NONE && (ty.is_ptr() || (ty.is_scalar() && (ty.is_int() || ty.is_float())))
}

#[derive(Clone, Copy)]
enum IfPattern {
    /// `b` branches on `cond` to `(arm, [])` and `(join, direct_args)`; `arm` jumps to `(join, arm_args)`.
    /// `arm_is_then == true` means `arm` is the `then` branch; `false` means `arm` is the `else` branch.
    Triangle {
        head: u32,
        cond: u32,
        arm: u32,
        join: u32,
        arm_is_then: bool,
    },
    /// `b` branches on `cond` to `(t_arm, [])` and `(e_arm, [])`; both jump to `join`.
    Diamond {
        head: u32,
        cond: u32,
        t_arm: u32,
        e_arm: u32,
        join: u32,
    },
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
    u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0)
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
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
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
        let at = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, at)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, at)
}

fn push_select<A: Allocator>(func: &mut Func<A>, ty: Ty, cond: u32, t_val: u32, f_val: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_val);
    func.extra.push(f_val);
    push_inst(func, Op::Select, Flags::NONE, ty, at)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) {
    let at = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, at);
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
                        let b = at_u32(view.extra, p);
                        let n_u32 = at_u32(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(b);
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
                            let b = at_u32(view.extra, p);
                            let an_u32 = at_u32(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(b);
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

/// Finds one pure triangle or diamond `if` eligible for branchless if-conversion.
fn find_if_pattern<A: Allocator + Copy>(view: &View<'_>, alloc: A) -> Option<IfPattern> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 3 {
        return None;
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
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
                if let Some(p) = preds.get_mut(tb) {
                    if p.scan_for(&b32).is_none() {
                        p.push(b32);
                    }
                }
            }
        });
    }

    let arm_ok = |head: u32, arm: u32| -> Option<(u32, usize)> {
        if arm == head || view.block_kind(Block(arm)) != BlockKind::Normal {
            return None;
        }
        let arm_preds = preds.get(to_idx(arm))?;
        if arm_preds.len() != 1 || arm_preds.first().copied() != Some(head) {
            return None;
        }
        if !view.params(Block(arm)).is_empty() {
            return None;
        }
        let range = view.block(Block(arm));
        let term = range.end - 1;
        if view.op(term) != Op::Jump {
            return None;
        }
        let body_len = term - range.start;
        if body_len > MAX_ARM_INSTS {
            return None;
        }
        for inst in range.start..term {
            if !is_speculable_pure(view.op(inst), Flags(at_u8(view.flags, inst))) {
                return None;
            }
        }
        // Ensure no instruction defined inside `arm` is used directly in any other block.
        for b in 0..blocks {
            let b32 = to_u32(b);
            if b32 == arm {
                continue;
            }
            for inst in view.block(Block(b32)) {
                let mut escaped = false;
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        if at_u32(&inst_block, to_idx(v)) == arm {
                            escaped = true;
                        }
                    }
                });
                if escaped {
                    return None;
                }
            }
        }
        let jat = to_idx(at_u32(view.data, term));
        let join = at_u32(view.extra, jat);
        if join == head || join == arm {
            return None;
        }
        Some((join, body_len))
    };

    for b in 0..blocks {
        let head = to_u32(b);
        if view.block_kind(Block(head)) != BlockKind::Normal {
            continue;
        }
        let term = view.terminator(Block(head));
        if view.op(term) != Op::Br {
            continue;
        }
        let at = to_idx(at_u32(view.data, term));
        let cond = at_u32(view.extra, at);
        let t_blk = at_u32(view.extra, at + 1);
        let t_argc = to_idx(at_u32(view.extra, at + 2));
        let e_at = at + 3 + t_argc;
        let e_blk = at_u32(view.extra, e_at);
        let e_argc = to_idx(at_u32(view.extra, e_at + 1));
        if t_blk == e_blk {
            continue;
        }

        // 1. Triangle: `head -> t_arm -> e_blk` (`t_argc == 0`)
        if t_argc == 0 {
            if let Some((join, _)) = arm_ok(head, t_blk) {
                if join == e_blk {
                    let params = view.params(Block(join));
                    if params.len() == e_argc
                        && (params.start..params.end).all(|p| is_selectable_ty(Ty(at_u16(view.tys, p))))
                    {
                        return Some(IfPattern::Triangle {
                            head,
                            cond,
                            arm: t_blk,
                            join,
                            arm_is_then: true,
                        });
                    }
                }
            }
        }

        // 2. Triangle: `head -> e_arm -> t_blk` (`e_argc == 0`)
        if e_argc == 0 {
            if let Some((join, _)) = arm_ok(head, e_blk) {
                if join == t_blk {
                    let params = view.params(Block(join));
                    if params.len() == t_argc
                        && (params.start..params.end).all(|p| is_selectable_ty(Ty(at_u16(view.tys, p))))
                    {
                        return Some(IfPattern::Triangle {
                            head,
                            cond,
                            arm: e_blk,
                            join,
                            arm_is_then: false,
                        });
                    }
                }
            }
        }

        // 3. Diamond: `head -> t_arm -> join` AND `head -> e_arm -> join` (`t_argc == 0 && e_argc == 0`)
        if t_argc == 0 && e_argc == 0 {
            if let (Some((j1, n1)), Some((j2, n2))) = (arm_ok(head, t_blk), arm_ok(head, e_blk)) {
                if j1 == j2 && n1 + n2 <= MAX_ARM_INSTS {
                    let params = view.params(Block(j1));
                    if (params.start..params.end).all(|p| is_selectable_ty(Ty(at_u16(view.tys, p)))) {
                        return Some(IfPattern::Diamond {
                            head,
                            cond,
                            t_arm: t_blk,
                            e_arm: e_blk,
                            join: j1,
                        });
                    }
                }
            }
        }
    }
    None
}

/// If `val` is `op(acc, x)` (or `op(x, acc)`) for an associative integer reduction `Add`/`Xor`/`Or`/`And`
/// with identity `id` (`0` for `Add`/`Xor`/`Or`, `!0` for `And`), returns `Some((op, x, id))`.
fn match_reduction_on_acc(func: &Func<impl Allocator>, val: u32, acc: u32, ty: Ty) -> Option<(Op, u32, u64)> {
    if !(ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64) {
        return None;
    }
    let vi = to_idx(val);
    let op = Op::from_byte(func.ops.get(vi).copied()?)?;
    if !matches!(op, Op::Add | Op::Xor | Op::Or | Op::And) {
        return None;
    }
    if op.info().flags.meanings_only(Flags(at_u8(&func.flags, vi))).0 != 0 {
        return None;
    }
    let at = to_idx(at_u32(&func.data, vi));
    let (l, r) = (at_u32(&func.extra, at), at_u32(&func.extra, at + 1));
    let other = if l == acc {
        r
    } else if r == acc {
        l
    } else {
        return None;
    };
    let id = if op == Op::And { u64::MAX } else { 0 };
    Some((op, other, id))
}

fn apply_if_pattern<A: Allocator + Copy>(
    view: &View<'_>,
    pat: IfPattern,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
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
    let mut canonicalized = 0u32;

    let head = match pat {
        IfPattern::Triangle { head, .. } | IfPattern::Diamond { head, .. } => head,
    };

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let term = range.end - 1;

        if b32 == head {
            for inst in range.start..term {
                let mapped = copy_remapped(view, inst, &val_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let at = to_idx(at_u32(view.data, term));
            let t_argc = to_idx(at_u32(view.extra, at + 2));
            let e_at = at + 3 + t_argc;
            let e_argc = to_idx(at_u32(view.extra, e_at + 1));

            let (cond_new, join, t_args_new, e_args_new) = match pat {
                IfPattern::Triangle { cond, arm, join, arm_is_then, .. } => {
                    let ar = view.block(Block(arm));
                    let aterm = ar.end - 1;
                    for inst in ar.start..aterm {
                        let mapped = copy_remapped(view, inst, &val_map, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                    let jat = to_idx(at_u32(view.data, aterm));
                    let argc = to_idx(at_u32(view.extra, jat + 1));
                    let mut arm_args: Vec<u32, A> = Vec::with_capacity_in(argc, alloc);
                    for k in 0..argc {
                        arm_args.push(at_u32(&val_map, to_idx(at_u32(view.extra, jat + 2 + k))));
                    }
                    let mut dir_args: Vec<u32, A> = Vec::with_capacity_in(argc, alloc);
                    if arm_is_then {
                        for k in 0..e_argc {
                            dir_args.push(at_u32(&val_map, to_idx(at_u32(view.extra, e_at + 2 + k))));
                        }
                        (at_u32(&val_map, to_idx(cond)), join, arm_args, dir_args)
                    } else {
                        for k in 0..t_argc {
                            dir_args.push(at_u32(&val_map, to_idx(at_u32(view.extra, at + 3 + k))));
                        }
                        (at_u32(&val_map, to_idx(cond)), join, dir_args, arm_args)
                    }
                }
                IfPattern::Diamond { cond, t_arm, e_arm, join, .. } => {
                    let tr = view.block(Block(t_arm));
                    let tterm = tr.end - 1;
                    for inst in tr.start..tterm {
                        let mapped = copy_remapped(view, inst, &val_map, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                    let er = view.block(Block(e_arm));
                    let eterm = er.end - 1;
                    for inst in er.start..eterm {
                        let mapped = copy_remapped(view, inst, &val_map, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                    let tjat = to_idx(at_u32(view.data, tterm));
                    let tlen = to_idx(at_u32(view.extra, tjat + 1));
                    let mut t_args: Vec<u32, A> = Vec::with_capacity_in(tlen, alloc);
                    for k in 0..tlen {
                        t_args.push(at_u32(&val_map, to_idx(at_u32(view.extra, tjat + 2 + k))));
                    }
                    let ejat = to_idx(at_u32(view.data, eterm));
                    let elen = to_idx(at_u32(view.extra, ejat + 1));
                    let mut e_args: Vec<u32, A> = Vec::with_capacity_in(elen, alloc);
                    for k in 0..elen {
                        e_args.push(at_u32(&val_map, to_idx(at_u32(view.extra, ejat + 2 + k))));
                    }
                    (at_u32(&val_map, to_idx(cond)), join, t_args, e_args)
                }
            };

            let j_params = view.params(Block(join));
            let mut merged: Vec<u32, A> = Vec::with_capacity_in(t_args_new.len(), alloc);
            for k in 0..t_args_new.len() {
                let tv = at_u32(&t_args_new, k);
                let ev = at_u32(&e_args_new, k);
                if tv == ev {
                    merged.push(tv);
                    continue;
                }
                let pty = Ty(at_u16(view.tys, j_params.start + k));
                // Canonicalize conditional reductions:
                //   `Select(c, acc + x, acc) -> acc + Select(c, x, 0)`
                //   `Select(c, acc, acc + x) -> acc + Select(c, 0, x)`
                if let Some((rop, x, id)) = match_reduction_on_acc(&func, tv, ev, pty) {
                    let id_c = push_iconst(&mut func, pty, id);
                    let masked_x = push_select(&mut func, pty, cond_new, x, id_c);
                    let upd = push_bin(&mut func, rop, Flags::NONE, pty, ev, masked_x);
                    merged.push(upd);
                    canonicalized += 1;
                } else if let Some((rop, x, id)) = match_reduction_on_acc(&func, ev, tv, pty) {
                    let id_c = push_iconst(&mut func, pty, id);
                    let masked_x = push_select(&mut func, pty, cond_new, id_c, x);
                    let upd = push_bin(&mut func, rop, Flags::NONE, pty, tv, masked_x);
                    merged.push(upd);
                    canonicalized += 1;
                } else {
                    merged.push(push_select(&mut func, pty, cond_new, tv, ev));
                }
            }
            push_jump(&mut func, join, &merged);
        } else {
            let is_dead_arm = match pat {
                IfPattern::Triangle { arm, .. } => b32 == arm,
                IfPattern::Diamond { t_arm, e_arm, .. } => b32 == t_arm || b32 == e_arm,
            };
            if is_dead_arm {
                // Now unreachable; `dead` + `coalesce_cfg` removes it immediately after.
                push_inst(&mut func, Op::Unreachable, Flags::NONE, Ty::NONE, 0);
            } else {
                for inst in range {
                    let mapped = copy_remapped(view, inst, &val_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    let d = dead(func.view(), alloc)?;
    let c = coalesce_cfg(d.view(), alloc)?;
    Ok((c, canonicalized))
}

/// Converts pure diamond and triangle branches into branchless `Op::Select`s (up to `max_converts`),
/// canonicalizing conditional reductions and coalescing resulting straight-line blocks.
pub fn if_convert<A: Allocator + Copy>(
    view: View<'_>,
    max_converts: u32,
    alloc: A,
) -> Result<IfConvOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut stats = IfConvStats::default();
    for _ in 0..max_converts {
        let Some(pat) = find_if_pattern(&cur.view(), alloc) else {
            break;
        };
        let (next, canon) = apply_if_pattern(&cur.view(), pat, alloc)?;
        cur = next;
        stats.branches_converted += 1;
        stats.reductions_canonicalized += canon;
    }
    Ok(IfConvOutput { func: cur, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        IntCond,
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::Conv,
    };

    #[test]
    fn if_convert_triangle_canonicalizes_conditional_reduction_and_matches_eval() {
        // `fn cond_sum(n: u64, thresh: u64) -> u64`:
        //   for i in 0..n { if i > thresh { acc += i; } }
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, hdr, body, then_b, join, exit] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
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
        let func = b.finish_checked().unwrap();

        let out = if_convert(func.view(), 4, TestHeap).unwrap();
        assert_eq!(out.stats.branches_converted, 1);
        assert_eq!(out.stats.reductions_canonicalized, 1);
        // Should collapse `body + then_b + join` into ONE block (`entry`, `hdr`, `body`, `exit` = 4 blocks)!
        assert_eq!(out.func.view().blocks(), 4);

        for &(nv, tv) in &[(0u128, 0u128), (10, 5), (20, 0), (20, 100), (17, 7)] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv), Val::Int(tv)], 10_000, TestHeap).unwrap() else {
                panic!("orig eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &[Val::Int(nv), Val::Int(tv)], 10_000, TestHeap).unwrap() else {
                panic!("ifconv eval");
            };
            assert_eq!(a, c, "if_convert mismatch at n={nv}, thresh={tv}");
        }
    }
}

//! Software-Pipelined Hardware Prefetcher (`insert_software_prefetch`) & `bstd` Compiler-Intrinsic
//! Builders (`build_swisstable_probe_16b`, `build_fast_utoa_u64`).
//!
//! 1. **Software-Pipelined Hardware Prefetcher (`insert_software_prefetch`)**: detects counted
//!    loops (`for i in init..n`) containing indirect/gather loads (`idx = table[i]; val =
//!    data[idx * scale]`) or strided loads (`data[i * stride]`), registers the
//!    `"blaze.prefetch.r1"` target intrinsic, and inserts clamped lookahead prefetches
//!    (`i_pf = umin(i + lookahead, n - 1)`) at the top of the loop body so L2/DRAM miss latency
//!    overlaps with current-iteration work without ever faulting past the array bound.
//! 2. **SwissTable 16-Byte Control-Group Probe (`build_swisstable_probe_16b`)**: builds a
//!    verified, branchless, 16-byte-parallel control-group probe `fn(ctrl: *const u8, h2: u8) -> u64`
//!    returning the exact 16-bit match mask (bit `k` set iff `ctrl[k] == h2`, zero cross-byte
//!    borrow, single-multiply bit-gather per 64-bit half) that executes identically in the LIR
//!    reference evaluator and native Sota codegen.
//! 3. **Zero-Allocation Fast `u64` Decimal Formatter (`build_fast_utoa_u64`)**: builds a verified
//!    `fn(val: u64, dst: *mut u8) -> u64` writing ASCII decimal digits directly into `dst` and
//!    returning the length `1..=20` with zero heap allocation (using `/ 10` and `% 10` so Sota
//!    lowers them via Granlund-Montgomery magic multiply with no hardware `udiv`).
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    builder::{Builder, Full, MemAttrs, Param, Variadic},
    func::{Conv, Ext, IntrinsicId, IntrinsicRec, ParamKind, ParamRec, SigRec},
    op::{Field, Layout},
    verify,
};

/// Target intrinsic name emitted for L1 read prefetching (`PRFM PLDL1KEEP` / `PREFETCHT0`).
pub const PREFETCH_READ_L1_INTRINSIC: &str = "blaze.prefetch.r1";

/// Maximum number of prefetch streams emitted per loop to avoid saturating L1 MSHRs.
const MAX_PREFETCHES_PER_LOOP: usize = 4;

/// Telemetry returned by [`insert_software_prefetch`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct PrefetchStats {
    pub loops_prefetched: u32,
    pub prefetches_inserted: u32,
}

/// Output of [`insert_software_prefetch`].
#[derive(Debug)]
pub struct PrefetchOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: PrefetchStats,
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
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn conv_byte(c: Conv) -> u8 {
    Conv::ALL.iter().position(|&x| x == c).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn ext_byte(e: Ext) -> u8 {
    Ext::ALL.iter().position(|&x| x == e).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn param_kind_byte(k: ParamKind) -> u8 {
    ParamKind::ALL.iter().position(|&x| x == k).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
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

/// Linear byte-offset scaling applied to an index value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IndexScale {
    /// `off = idx` (byte stride 1).
    Unit,
    /// `off = idx << shift`.
    Shl(u64),
    /// `off = idx * factor`.
    Mul(u64),
}

/// Optional integer widening between a loaded table index and its byte-offset scaling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExtKind {
    None,
    Zext,
    Sext,
}

/// A strided load `Load(PtrAdd(base, iv * scale), load_off)` inside a counted loop body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StridedLoad {
    load_inst: u32,
    base: u32,
    scale: IndexScale,
    load_off: i32,
    elem_ty: Ty,
}

/// An indirect/gather load `Load(PtrAdd(data_base, scale(ext(table[i]))), data_load_off)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct GatherLoad {
    table_idx: usize,
    data_base: u32,
    ext: ExtKind,
    scale_ty: Ty,
    data_scale: IndexScale,
    data_load_off: i32,
}

/// Prefetch plan attached to a single counted loop body.
struct LoopPrefetchPlan<A: Allocator> {
    body: u32,
    iv: u32,
    bound: u32,
    lookahead_delta: u64,
    strided: Vec<StridedLoad, A>,
    gathers: Vec<GatherLoad, A>,
}

/// Checks whether `val` is `iv`, `iv << k`, or `iv * c` (for `c >= 1`).
fn decompose_iv_scale(view: &View<'_>, val: u32, iv: u32) -> Option<IndexScale> {
    if val == iv {
        return Some(IndexScale::Unit);
    }
    let vi = to_idx(val);
    match view.op(vi) {
        Op::Shl => {
            let at = to_idx(at_u32(view.data, vi));
            let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
            if lhs == iv {
                let k = const_u64(view, to_idx(rhs))?;
                if k < 64 {
                    return Some(if k == 0 { IndexScale::Unit } else { IndexScale::Shl(k) });
                }
            }
            None
        }
        Op::Mul => {
            let at = to_idx(at_u32(view.data, vi));
            let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
            if lhs == iv {
                let c = const_u64(view, to_idx(rhs))?;
                if c >= 1 {
                    return Some(if c == 1 { IndexScale::Unit } else { IndexScale::Mul(c) });
                }
            } else if rhs == iv {
                let c = const_u64(view, to_idx(lhs))?;
                if c >= 1 {
                    return Some(if c == 1 { IndexScale::Unit } else { IndexScale::Mul(c) });
                }
            }
            None
        }
        _ => None,
    }
}

/// Checks whether `val` is derived from one of `strided` loads via optional `zext`/`sext` and
/// optional `shl`/`mul` scaling.
fn decompose_gather_offset(
    view: &View<'_>,
    val: u32,
    strided: &[StridedLoad],
) -> Option<(usize, ExtKind, Ty, IndexScale)> {
    let vi = to_idx(val);
    let scale_ty = Ty(at_u16(view.tys, vi));
    if !scale_ty.is_scalar() || !scale_ty.is_int() {
        return None;
    }

    // Strip optional outer `shl` or `mul` by constant.
    let (inner, data_scale) = match view.op(vi) {
        Op::Shl => {
            let at = to_idx(at_u32(view.data, vi));
            let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
            if let Some(k) = const_u64(view, to_idx(rhs)).filter(|&k| k < 64) {
                (lhs, if k == 0 { IndexScale::Unit } else { IndexScale::Shl(k) })
            } else {
                (val, IndexScale::Unit)
            }
        }
        Op::Mul => {
            let at = to_idx(at_u32(view.data, vi));
            let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
            if let Some(c) = const_u64(view, to_idx(rhs)).filter(|&c| c >= 1) {
                (lhs, if c == 1 { IndexScale::Unit } else { IndexScale::Mul(c) })
            } else if let Some(c) = const_u64(view, to_idx(lhs)).filter(|&c| c >= 1) {
                (rhs, if c == 1 { IndexScale::Unit } else { IndexScale::Mul(c) })
            } else {
                (val, IndexScale::Unit)
            }
        }
        _ => (val, IndexScale::Unit),
    };

    // Strip optional `zext` / `sext`.
    let inner_i = to_idx(inner);
    let (src, ext) = match view.op(inner_i) {
        Op::Zext => (at_u32(view.data, inner_i), ExtKind::Zext),
        Op::Sext => (at_u32(view.data, inner_i), ExtKind::Sext),
        _ => (inner, ExtKind::None),
    };

    let table_idx = strided.iter().position(|s| s.load_inst == src)?;
    let tbl = strided.get(table_idx)?;
    if !tbl.elem_ty.is_scalar() || !tbl.elem_ty.is_int() {
        return None;
    }
    Some((table_idx, ext, scale_ty, data_scale))
}

fn push_inst<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, data: u32, attr: u32) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(0);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64) -> u32 {
    if ty.bits(64) <= 32 {
        let mask = if ty.bits(64) == 32 { u64::from(u32::MAX) } else { (1u64 << ty.bits(64)) - 1 };
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val & mask), 0)
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned(), 0)
    } else {
        let at = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, at, 0)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, at, 0)
}

fn push_load<A: Allocator>(func: &mut Func<A>, ty: Ty, ptr: u32, off: i32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(ptr);
    func.extra.push(off.cast_unsigned());
    push_inst(func, Op::Load, Flags::NONE, ty, at, 0)
}

fn push_prefetch<A: Allocator>(func: &mut Func<A>, intrinsic: IntrinsicId, ptr: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(intrinsic.0);
    func.extra.push(1);
    func.extra.push(ptr);
    push_inst(func, Op::TargetIntrinsic, Flags::NONE, Ty::NONE, at, 0)
}

fn emit_scaled_offset<A: Allocator>(func: &mut Func<A>, idx: u32, ty: Ty, scale: IndexScale) -> u32 {
    match scale {
        IndexScale::Unit => idx,
        IndexScale::Shl(k) => {
            let ck = push_iconst(func, ty, k);
            push_bin(func, Op::Shl, Flags::NONE, ty, idx, ck)
        }
        IndexScale::Mul(c) => {
            let cc = push_iconst(func, ty, c);
            push_bin(func, Op::Mul, Flags::NONE, ty, idx, cc)
        }
    }
}

fn emit_addr_with_off<A: Allocator>(func: &mut Func<A>, base: u32, off_val: u32, load_off: i32) -> u32 {
    let ptr = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, base, off_val);
    if load_off != 0 {
        let extra = push_iconst(func, Ty::I64, i64::from(load_off).cast_unsigned());
        push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, ptr, extra)
    } else {
        ptr
    }
}

/// Ensures `"blaze.prefetch.r1"` with signature `(ptr) -> ()` exists in `func.intrinsics`,
/// preserving canonical CSR tiling of `sigs`/`sig_params` and `intrinsics`/`names`.
fn ensure_prefetch_intrinsic<A: Allocator>(func: &mut Func<A>) -> IntrinsicId {
    let want = PREFETCH_READ_L1_INTRINSIC.as_bytes();
    for (row, rec) in func.intrinsics.iter().enumerate() {
        let start = to_idx(rec.name_first);
        let Some(end) = start.checked_add(to_idx(rec.name_len)) else {
            continue;
        };
        if func.names.get(start..end) == Some(want) {
            if let Some(&sig) = func.sigs.get(to_idx(rec.sig)) {
                if sig.params == 1
                    && sig.rets == 0
                    && sig.bits == 0
                    && func.sig_params.get(to_idx(sig.first)).map(|p| p.ty) == Some(Ty::PTR.0)
                {
                    return IntrinsicId(to_u32(row));
                }
            }
        }
    }

    let c_conv = conv_byte(Conv::C);
    let no_ext = ext_byte(Ext::None);
    let val_kind = param_kind_byte(ParamKind::Value);

    // Reuse an existing `(ptr) -> ()` C signature if present; otherwise append one.
    let existing_sig = func.sigs.iter().enumerate().find_map(|(r, s)| {
        (s.conv == c_conv
            && s.bits == 0
            && s.params == 1
            && s.rets == 0
            && func.sig_params.get(to_idx(s.first)).copied()
                == Some(ParamRec {
                    facts: 0,
                    ty: Ty::PTR.0,
                    ext: no_ext,
                    kind: val_kind,
                }))
        .then_some(to_u32(r))
    });
    let sig_row = match existing_sig {
        Some(row) => row,
        None => {
            let first = to_u32(func.sig_params.len());
            func.sig_params.push(ParamRec {
                facts: 0,
                ty: Ty::PTR.0,
                ext: no_ext,
                kind: val_kind,
            });
            let row = to_u32(func.sigs.len());
            func.sigs.push(SigRec {
                first,
                params: 1,
                rets: 0,
                conv: c_conv,
                bits: 0,
                pad: [0; 2],
            });
            row
        }
    };

    let name_first = to_u32(func.names.len());
    func.names.extend_from_slice(want);
    let id = to_u32(func.intrinsics.len());
    func.intrinsics.push(IntrinsicRec {
        name_first,
        name_len: to_u32(want.len()),
        sig: sig_row,
    });
    IntrinsicId(id)
}

/// A deferred value-operand fixup (only needed when a CFG block appears before its dominator in
/// layout order).
#[derive(Clone, Copy)]
enum ValFixup {
    Data { inst: usize, old_val: u32 },
    Extra { pos: usize, old_val: u32 },
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let new_inst = func.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let old = inst_data;
            let mapped = at_u32(val_map, to_idx(old));
            if mapped == NONE {
                fixups.push(ValFixup::Data { inst: new_inst, old_val: old });
                0
            } else {
                mapped
            }
        }
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        let old = at_u32(view.extra, p);
                        let mapped = at_u32(val_map, to_idx(old));
                        if mapped == NONE {
                            fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                            func.extra.push(0);
                        } else {
                            func.extra.push(mapped);
                        }
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let old = at_u32(view.extra, p + k);
                            let mapped = at_u32(val_map, to_idx(old));
                            if mapped == NONE {
                                fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                func.extra.push(0);
                            } else {
                                func.extra.push(mapped);
                            }
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
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
                            let old = at_u32(view.extra, p + k);
                            let mapped = at_u32(val_map, to_idx(old));
                            if mapped == NONE {
                                fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                func.extra.push(0);
                            } else {
                                func.extra.push(mapped);
                            }
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
                                let old = at_u32(view.extra, p + k);
                                let mapped = at_u32(val_map, to_idx(old));
                                if mapped == NONE {
                                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                    func.extra.push(0);
                                } else {
                                    func.extra.push(mapped);
                                }
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
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
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    to_u32(new_inst)
}

/// Computes immediate dominators (`idom[b]`, `NONE` for unreachable blocks) using Cooper, Harvey,
/// and Kennedy's iterative algorithm.
fn compute_idom<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Vec<u32, A> {
    let blocks = view.blocks();
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        succs.push(Vec::new_in(*alloc));
        preds.push(Vec::new_in(*alloc));
    }
    for b in 0..blocks {
        let b32 = to_u32(b);
        each(view, view.terminator(Block(b32)), |item| {
            if let Item::Edge(edge) = item {
                if let Some(sb) = succs.get_mut(b) {
                    sb.push(edge.block);
                }
                if let Some(pe) = preds.get_mut(to_idx(edge.block)) {
                    pe.push(b32);
                }
            }
        });
    }

    // Postorder traversal from entry block 0.
    let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut seen = filled(blocks, false, alloc);
    let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, *alloc);
    set_at(&mut seen, 0, true);
    stack.push((0, 0));
    while let Some(top) = stack.last_mut() {
        let (b, next) = *top;
        if let Some(&s) = succs.get(to_idx(b)).and_then(|v| v.get(next)) {
            top.1 += 1;
            let si = to_idx(s);
            if !at_bool(&seen, si) {
                set_at(&mut seen, si, true);
                stack.push((s, 0));
            }
        } else {
            post.push(b);
            stack.pop();
        }
    }

    let mut order = filled(blocks, NONE, alloc);
    let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), *alloc);
    for (idx, &b) in post.iter().rev().enumerate() {
        set_at(&mut order, to_idx(b), to_u32(idx));
        rpo.push(b);
    }

    let mut idom = filled(blocks, NONE, alloc);
    set_at(&mut idom, 0, 0);
    for _ in 0..2 * blocks + 2 {
        let mut changed = false;
        for &b in rpo.iter().skip(1) {
            let bi = to_idx(b);
            let mut new_idom = NONE;
            if let Some(b_preds) = preds.get(bi) {
                for &p in b_preds.iter() {
                    if at_u32(&idom, to_idx(p)) == NONE {
                        continue;
                    }
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        let mut a = p;
                        let mut c = new_idom;
                        for _ in 0..2 * blocks + 2 {
                            if a == c {
                                break;
                            }
                            while at_u32(&order, to_idx(a)) > at_u32(&order, to_idx(c)) {
                                a = at_u32(&idom, to_idx(a));
                            }
                            while at_u32(&order, to_idx(c)) > at_u32(&order, to_idx(a)) {
                                c = at_u32(&idom, to_idx(c));
                            }
                        }
                        a
                    };
                }
            }
            if new_idom != at_u32(&idom, bi) {
                set_at(&mut idom, bi, new_idom);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    idom
}

/// Whether reachable block `a` dominates reachable block `b`.
fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
    if at_u32(idom, to_idx(a)) == NONE || at_u32(idom, to_idx(b)) == NONE {
        return false;
    }
    for _ in 0..idom.len() {
        if a == b {
            return true;
        }
        if b == 0 {
            return false;
        }
        b = at_u32(idom, to_idx(b));
    }
    false
}

/// Inserts software-pipelined L1 read prefetches (`"blaze.prefetch.r1"`) into counted loops with
/// indirect/gather or strided loads, clamping lookahead indices to `[0, n)` via
/// `i_pf = umin(i + lookahead, n - 1)` so trailing iterations never fault.
pub fn insert_software_prefetch<A: Allocator + Copy>(
    view: View<'_>,
    lookahead: u64,
    alloc: A,
) -> Result<PrefetchOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if lookahead == 0 || len < 4 || view.head.ptr_bits != 64 {
        return Ok(PrefetchOutput { func: view.to_func_in(alloc), stats: PrefetchStats::default() });
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut pred_counts = filled(blocks, 0u32, &alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut inst_block, inst, b32);
        }
        each(&view, view.terminator(Block(b32)), |item| {
            if let Item::Edge(edge) = item {
                if let Some(pc) = pred_counts.get_mut(to_idx(edge.block)) {
                    *pc += 1;
                }
            }
        });
    }
    let idom = compute_idom(&view, &alloc);

    let mut plans: Vec<LoopPrefetchPlan<A>, A> = Vec::new_in(alloc);
    let mut planned_body = filled(blocks, false, &alloc);

    for hdr_idx in 0..blocks {
        let hdr = to_u32(hdr_idx);
        if at_u32(&idom, hdr_idx) == NONE {
            continue;
        }
        let hparams = view.params(Block(hdr));
        if hparams.is_empty() {
            continue;
        }
        let hterm = view.terminator(Block(hdr));
        if view.op(hterm) != Op::Br {
            continue;
        }

        // Decode `br cond, t_blk(...), e_blk(...)`.
        let bat = to_idx(at_u32(view.data, hterm));
        let cond_v = to_idx(at_u32(view.extra, bat));
        let t_blk = at_u32(view.extra, bat + 1);
        let t_n = to_idx(at_u32(view.extra, bat + 2));
        let e_blk = at_u32(view.extra, bat + 3 + t_n);
        if view.op(cond_v) != Op::Icmp {
            continue;
        }
        let iat = to_idx(at_u32(view.data, cond_v));
        let Some(icond) = IntCond::from_word(at_u32(view.extra, iat)) else { continue };
        let (lhs, rhs) = (at_u32(view.extra, iat + 1), at_u32(view.extra, iat + 2));

        // Match counted loop guard `iv < bound` (unsigned or signed, either branch polarity).
        let is_hdr_i64_param = |v: u32| {
            let vi = to_idx(v);
            vi >= hparams.start && vi < hparams.end && Ty(at_u16(view.tys, vi)) == Ty::I64
        };
        let (iv, bound, body) = match icond {
            IntCond::Ult | IntCond::Slt if is_hdr_i64_param(lhs) => (lhs, rhs, t_blk),
            IntCond::Ugt | IntCond::Sgt if is_hdr_i64_param(rhs) => (rhs, lhs, t_blk),
            IntCond::Uge | IntCond::Sge if is_hdr_i64_param(lhs) => (lhs, rhs, e_blk),
            IntCond::Ule | IntCond::Sle if is_hdr_i64_param(rhs) => (rhs, lhs, e_blk),
            _ => continue,
        };

        let body_i = to_idx(body);
        if body == hdr || at_bool(&planned_body, body_i) {
            continue;
        }
        let bound_b = at_u32(&inst_block, to_idx(bound));
        // Require `body` to be layout-after `hdr` with single predecessor `hdr` and no block params,
        // and `bound` strictly dominating `hdr` (and defined before `body` in layout order).
        if body <= hdr
            || at_u32(&pred_counts, body_i) != 1
            || !view.params(Block(body)).is_empty()
            || bound_b == hdr
            || !dominates(&idom, bound_b, hdr)
            || bound_b >= body
        {
            continue;
        }

        // Locate the latch jumping back to `hdr` (either `body` itself or a single-pred latch block).
        let bterm = view.terminator(Block(body));
        if view.op(bterm) != Op::Jump {
            continue;
        }
        let bjat = to_idx(at_u32(view.data, bterm));
        let b_target = at_u32(view.extra, bjat);
        let (latch, latch_jump) = if b_target == hdr {
            (body, bterm)
        } else if at_u32(&pred_counts, to_idx(b_target)) == 1 && view.params(Block(b_target)).is_empty() {
            let lterm = view.terminator(Block(b_target));
            if view.op(lterm) == Op::Jump && at_u32(view.extra, to_idx(at_u32(view.data, lterm))) == hdr {
                (b_target, lterm)
            } else {
                continue;
            }
        } else {
            continue;
        };

        // Verify the backedge increments `iv` by a positive constant `step`.
        let iv_pos = to_idx(iv) - hparams.start;
        let ljat = to_idx(at_u32(view.data, latch_jump));
        let arg_count = to_idx(at_u32(view.extra, ljat + 1));
        if iv_pos >= arg_count {
            continue;
        }
        let iv_next = to_idx(at_u32(view.extra, ljat + 2 + iv_pos));
        if view.op(iv_next) != Op::Add {
            continue;
        }
        let aat = to_idx(at_u32(view.data, iv_next));
        let (al, ar) = (at_u32(view.extra, aat), at_u32(view.extra, aat + 1));
        let step = if al == iv {
            const_u64(&view, to_idx(ar))
        } else if ar == iv {
            const_u64(&view, to_idx(al))
        } else {
            None
        };
        let Some(step) = step.filter(|&s| s >= 1) else { continue };

        let brange = view.block(Block(body));
        // Skip loops that already carry a software prefetch intrinsic (idempotent).
        let already_prefetched = (brange.start..bterm).any(|inst| {
            view.op(inst) == Op::TargetIntrinsic
                && view.intrinsic_name(IntrinsicId(at_u32(view.extra, to_idx(at_u32(view.data, inst)))))
                    == PREFETCH_READ_L1_INTRINSIC.as_bytes()
        });
        if already_prefetched {
            continue;
        }

        // Scan `body` for strided loads `table[i * stride]` and gather loads `data[table[i] * scale]`.
        let mut strided: Vec<StridedLoad, A> = Vec::new_in(alloc);
        let mut gathers: Vec<GatherLoad, A> = Vec::new_in(alloc);

        let is_invariant_base = |base: u32| -> bool {
            let bb = at_u32(&inst_block, to_idx(base));
            bb != hdr && bb != body && bb != latch && bb < body && dominates(&idom, bb, hdr)
        };

        for inst in brange.start..bterm {
            if view.op(inst) != Op::Load || Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
                continue;
            }
            let lat = to_idx(at_u32(view.data, inst));
            let ptr_v = to_idx(at_u32(view.extra, lat));
            let load_off = at_u32(view.extra, lat + 1).cast_signed();
            if view.op(ptr_v) != Op::PtrAdd {
                continue;
            }
            let pat = to_idx(at_u32(view.data, ptr_v));
            let (base, off_v) = (at_u32(view.extra, pat), at_u32(view.extra, pat + 1));
            if !is_invariant_base(base) {
                continue;
            }

            if let Some(scale) = decompose_iv_scale(&view, off_v, iv) {
                strided.push(StridedLoad {
                    load_inst: to_u32(inst),
                    base,
                    scale,
                    load_off,
                    elem_ty: Ty(at_u16(view.tys, inst)),
                });
            } else if step == 1 {
                if let Some((table_idx, ext, scale_ty, data_scale)) =
                    decompose_gather_offset(&view, off_v, &strided)
                {
                    let dup = gathers.iter().any(|g| {
                        g.table_idx == table_idx
                            && g.data_base == base
                            && g.ext == ext
                            && g.data_scale == data_scale
                            && g.data_load_off == load_off
                    });
                    if !dup {
                        gathers.push(GatherLoad {
                            table_idx,
                            data_base: base,
                            ext,
                            scale_ty,
                            data_scale,
                            data_load_off: load_off,
                        });
                    }
                }
            }
        }

        if strided.is_empty() && gathers.is_empty() {
            continue;
        }

        set_at(&mut planned_body, body_i, true);
        plans.push(LoopPrefetchPlan {
            body,
            iv,
            bound,
            lookahead_delta: lookahead.saturating_mul(step),
            strided,
            gathers,
        });
    }

    if plans.is_empty() {
        return Ok(PrefetchOutput { func: view.to_func_in(alloc), stats: PrefetchStats::default() });
    }

    // Rebuild instruction columns, inserting clamped lookahead prefetches at each planned body head.
    let mut func = view.to_func_in(alloc);
    let prefetch_id = ensure_prefetch_intrinsic(&mut func);

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
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);
    let mut stats = PrefetchStats::default();

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));

        if let Some(plan) = plans.iter().find(|p| p.body == b32) {
            let iv_new = at_u32(&val_map, to_idx(plan.iv));
            let bound_new = at_u32(&val_map, to_idx(plan.bound));

            // Compute clamped lookahead index `i_pf = umin(i + lookahead_delta, bound - 1)`.
            // Inside `body` we are guarded by `i < bound`, so `bound >= 1` and `0 <= i_pf < bound`.
            let k_la = push_iconst(&mut func, Ty::I64, plan.lookahead_delta);
            let i_plus = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_new, k_la);
            let k_one = push_iconst(&mut func, Ty::I64, 1);
            let last_idx = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, bound_new, k_one);
            let i_pf = push_bin(&mut func, Op::Umin, Flags::NONE, Ty::I64, i_plus, last_idx);

            let mut inserted_here = 0usize;
            let mut strided_handled = filled(plan.strided.len(), false, &alloc);

            // 1. Prioritize indirect gather prefetches (`&table[i_pf]` + `&data[table[i_pf] * scale]`).
            for g in plan.gathers.iter() {
                if inserted_here >= MAX_PREFETCHES_PER_LOOP {
                    break;
                }
                let Some(&tbl) = plan.strided.get(g.table_idx) else {
                    continue;
                };
                let tbl_base = at_u32(&val_map, to_idx(tbl.base));
                let t_off = emit_scaled_offset(&mut func, i_pf, Ty::I64, tbl.scale);
                let t_ptr = emit_addr_with_off(&mut func, tbl_base, t_off, tbl.load_off);

                if !at_bool(&strided_handled, g.table_idx) && inserted_here < MAX_PREFETCHES_PER_LOOP {
                    push_prefetch(&mut func, prefetch_id, t_ptr);
                    set_at(&mut strided_handled, g.table_idx, true);
                    inserted_here += 1;
                }

                if inserted_here < MAX_PREFETCHES_PER_LOOP {
                    let idx_pf = push_load(&mut func, tbl.elem_ty, t_ptr, 0);
                    let idx_ext = match g.ext {
                        ExtKind::None => idx_pf,
                        ExtKind::Zext => {
                            push_inst(&mut func, Op::Zext, Flags::NONE, g.scale_ty, idx_pf, 0)
                        }
                        ExtKind::Sext => {
                            push_inst(&mut func, Op::Sext, Flags::NONE, g.scale_ty, idx_pf, 0)
                        }
                    };
                    let d_off = emit_scaled_offset(&mut func, idx_ext, g.scale_ty, g.data_scale);
                    let d_base = at_u32(&val_map, to_idx(g.data_base));
                    let d_ptr = emit_addr_with_off(&mut func, d_base, d_off, g.data_load_off);
                    push_prefetch(&mut func, prefetch_id, d_ptr);
                    inserted_here += 1;
                }
            }

            // 2. Emit prefetches for remaining independent strided loads (deduplicated by address).
            for (si, s) in plan.strided.iter().enumerate() {
                if inserted_here >= MAX_PREFETCHES_PER_LOOP {
                    break;
                }
                if at_bool(&strided_handled, si) {
                    continue;
                }
                let dup = (0..si).any(|prev| {
                    at_bool(&strided_handled, prev)
                        && plan.strided.get(prev).is_some_and(|p| {
                            p.base == s.base && p.scale == s.scale && p.load_off == s.load_off
                        })
                });
                if dup {
                    set_at(&mut strided_handled, si, true);
                    continue;
                }
                let s_base = at_u32(&val_map, to_idx(s.base));
                let s_off = emit_scaled_offset(&mut func, i_pf, Ty::I64, s.scale);
                let s_ptr = emit_addr_with_off(&mut func, s_base, s_off, s.load_off);
                push_prefetch(&mut func, prefetch_id, s_ptr);
                set_at(&mut strided_handled, si, true);
                inserted_here += 1;
            }

            if inserted_here > 0 {
                stats.loops_prefetched += 1;
                stats.prefetches_inserted += to_u32(inserted_here);
            }
        }

        for inst in range {
            let mapped = copy_remapped(&view, inst, &val_map, &mut func, &mut fixups);
            set_at(&mut val_map, inst, mapped);
        }
    }

    for fixup in fixups.iter() {
        match *fixup {
            ValFixup::Data { inst, old_val } => set_at(&mut func.data, inst, at_u32(&val_map, to_idx(old_val))),
            ValFixup::Extra { pos, old_val } => set_at(&mut func.extra, pos, at_u32(&val_map, to_idx(old_val))),
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(PrefetchOutput { func, stats })
}

/// Builds a verified LIR function `fn swisstable_probe_16b(ctrl: *const u8, h2: u8) -> u64`
/// returning the exact 16-bit match mask (bit `k` set iff `ctrl[k] == h2` for `k in 0..16`).
///
/// Uses branchless 64-bit SWAR parallel byte matching across two 8-byte halves with zero
/// cross-byte borrow:
/// - `x = word ^ (h2 * 0x0101_0101_0101_0101)`
/// - `m = !(((x & 0x7F7F...) + 0x7F7F...) | x) & 0x8080...` (bit `8k + 7` is 1 iff byte `k == h2`)
/// - `bits = (m * 0x0002_0408_1020_4081) >> 56` (exact 8-bit mask in 1 multiply + 1 shift)
///
/// Runs identically in both `eval::run_calls_in` and native AArch64/x86-64 Sota codegen.
pub fn build_swisstable_probe_16b<A: Allocator + Copy>(
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::I8)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);

    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let ctrl = b.param(Ty::PTR);
    let h2 = b.param(Ty::I8);

    // Load the 16 control bytes as two unaligned 64-bit little-endian words (align = 1 byte).
    let lo = b.load(Ty::I64, Flags::NONE, ctrl, 0, MemAttrs::default());
    let hi = b.load(Ty::I64, Flags::NONE, ctrl, 8, MemAttrs::default());

    // Broadcast `h2` across all 8 lanes of an `i64`.
    let ones = b.iconst(Ty::I64, 0x0101_0101_0101_0101u128);
    let mask7f = b.iconst(Ty::I64, 0x7F7F_7F7F_7F7F_7F7Fu128);
    let mask80 = b.iconst(Ty::I64, 0x8080_8080_8080_8080u128);
    let gather = b.iconst(Ty::I64, 0x0002_0408_1020_4081u128);
    let sh56 = b.iconst(Ty::I64, 56);
    let sh8 = b.iconst(Ty::I64, 8);

    let h2_64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, h2);
    let needle = b.binary(Op::Mul, Flags::NONE, h2_64, ones);

    // Low 8 control bytes -> bits 0..8.
    let lo_x = b.binary(Op::Xor, Flags::NONE, lo, needle);
    let lo_low = b.binary(Op::And, Flags::NONE, lo_x, mask7f);
    let lo_sum = b.binary(Op::Add, Flags::NONE, lo_low, mask7f);
    let lo_nz = b.binary(Op::Or, Flags::NONE, lo_sum, lo_x);
    let lo_inv = b.unary(Op::Not, Flags::NONE, lo_nz);
    let lo_msb = b.binary(Op::And, Flags::NONE, lo_inv, mask80);
    let lo_pack = b.binary(Op::Mul, Flags::NONE, lo_msb, gather);
    let lo_bits = b.binary(Op::Lshr, Flags::NONE, lo_pack, sh56);

    // High 8 control bytes -> bits 8..16.
    let hi_x = b.binary(Op::Xor, Flags::NONE, hi, needle);
    let hi_low = b.binary(Op::And, Flags::NONE, hi_x, mask7f);
    let hi_sum = b.binary(Op::Add, Flags::NONE, hi_low, mask7f);
    let hi_nz = b.binary(Op::Or, Flags::NONE, hi_sum, hi_x);
    let hi_inv = b.unary(Op::Not, Flags::NONE, hi_nz);
    let hi_msb = b.binary(Op::And, Flags::NONE, hi_inv, mask80);
    let hi_pack = b.binary(Op::Mul, Flags::NONE, hi_msb, gather);
    let hi_bits = b.binary(Op::Lshr, Flags::NONE, hi_pack, sh56);

    let hi_shifted = b.binary(Op::Shl, Flags::NONE, hi_bits, sh8);
    let mask16 = b.binary(Op::Or, Flags::NONE, lo_bits, hi_shifted);
    b.ret(&[mask16]);

    b.finish_checked().map_err(SimplifyError::Build)
}

/// Builds a verified zero-allocation LIR function `fn fast_utoa_u64(val: u64, dst: *mut u8) -> u64`
/// that formats `val` in base-10 ASCII into `dst` (caller provides >= 20 writable bytes) and
/// returns the digit count `1..=20`.
///
/// Uses a do-while `/ 10` + `% 10` digit-extraction loop followed by an in-place byte reversal so
/// Sota automatically strength-reduces the constant division by 10 to a Granlund-Montgomery magic
/// multiply with zero hardware `udiv` instructions.
pub fn build_fast_utoa_u64<A: Allocator + Copy>(alloc: A) -> Result<Func<A>, SimplifyError> {
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::PTR)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);

    let [entry, write_loop, rev_init, rev_hdr, rev_body, done] =
        [(); 6].map(|_| b.create_block(BlockKind::Normal));

    b.begin(entry);
    let val = b.param(Ty::I64);
    let dst = b.param(Ty::PTR);
    let zero = b.iconst(Ty::I64, 0);
    let one = b.iconst(Ty::I64, 1);
    let ten = b.iconst(Ty::I64, 10);
    let ascii0 = b.iconst(Ty::I8, u128::from(b'0'));
    b.jump((write_loop, &[val, zero]));

    // Extract digits least-significant first (do-while handles `val == 0` in 1 iteration).
    b.begin(write_loop);
    let cur = b.param(Ty::I64);
    let len = b.param(Ty::I64);
    let q = b.binary(Op::Udiv, Flags::NONE, cur, ten);
    let r = b.binary(Op::Urem, Flags::NONE, cur, ten);
    let r8 = b.convert(Op::Trunc, Flags::NONE, Ty::I8, r);
    let ch = b.binary(Op::Add, Flags::NONE, r8, ascii0);
    let p_out = b.ptr_add(Flags::NONE, dst, len);
    b.store(Flags::NONE, ch, p_out, 0, MemAttrs::default());
    let len2 = b.binary(Op::Add, Flags::NONE, len, one);
    let more = b.icmp(IntCond::Ne, q, zero);
    b.br(Flags::NONE, more, (write_loop, &[q, len2]), (rev_init, &[len2]));

    // Reverse `dst[0..total_len]` in place.
    b.begin(rev_init);
    let total_len = b.param(Ty::I64);
    let hi_init = b.binary(Op::Sub, Flags::NONE, total_len, one);
    b.jump((rev_hdr, &[zero, hi_init]));

    b.begin(rev_hdr);
    let lo = b.param(Ty::I64);
    let hi = b.param(Ty::I64);
    let need_swap = b.icmp(IntCond::Ult, lo, hi);
    b.br(Flags::NONE, need_swap, (rev_body, &[]), (done, &[]));

    b.begin(rev_body);
    let p_lo = b.ptr_add(Flags::NONE, dst, lo);
    let p_hi = b.ptr_add(Flags::NONE, dst, hi);
    let b_lo = b.load(Ty::I8, Flags::NONE, p_lo, 0, MemAttrs::default());
    let b_hi = b.load(Ty::I8, Flags::NONE, p_hi, 0, MemAttrs::default());
    b.store(Flags::NONE, b_hi, p_lo, 0, MemAttrs::default());
    b.store(Flags::NONE, b_lo, p_hi, 0, MemAttrs::default());
    let lo2 = b.binary(Op::Add, Flags::NONE, lo, one);
    let hi2 = b.binary(Op::Sub, Flags::NONE, hi, one);
    b.jump((rev_hdr, &[lo2, hi2]));

    b.begin(done);
    b.ret(&[total_len]);

    b.finish_checked().map_err(SimplifyError::Build)
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        eval::{Bodies, BodyId, Outcome, Val, run_calls_in},
        func::{RefId, RefKind, SlotKind},
    };

    const CALLER_ID: BodyId = BodyId([1; 32]);
    const CALLEE_ID: BodyId = BodyId([2; 32]);
    const FUEL: u64 = 100_000;

    struct SingleCallee<'a> {
        func: &'a Func<TestHeap>,
    }

    impl Bodies for SingleCallee<'_> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            match (from, reference) {
                (CALLER_ID, RefId(0)) => Some((CALLEE_ID, self.func.view())),
                _ => None,
            }
        }
    }

    #[test]
    fn software_prefetch_inserts_guarded_prefetch_in_gather_loop() {
        // `fn gather_sum(table: *const u64, data: *const u64, n: u64) -> u64`
        //   for i in 0..n { acc += data[table[i]]; }
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let table = b.param(Ty::PTR);
        let data = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[zero, zero]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let sh3 = b.iconst(Ty::I64, 3);
        let t_off = b.binary(Op::Shl, Flags::NONE, i, sh3);
        let t_ptr = b.ptr_add(Flags::NONE, table, t_off);
        let idx = b.load(Ty::I64, Flags::NONE, t_ptr, 0, MemAttrs::aligned(3));
        let d_off = b.binary(Op::Shl, Flags::NONE, idx, sh3);
        let d_ptr = b.ptr_add(Flags::NONE, data, d_off);
        let val = b.load(Ty::I64, Flags::NONE, d_ptr, 0, MemAttrs::aligned(3));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, val);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let out = insert_software_prefetch(func.view(), 4, TestHeap).unwrap();
        verify(out.func.view()).expect("prefetched gather loop must verify");
        assert_eq!(out.stats.loops_prefetched, 1);
        assert_eq!(
            out.stats.prefetches_inserted, 2,
            "expected prefetches for both &table[i_pf] and &data[table[i_pf]]"
        );

        let v = out.func.view();
        let pf_count = (0..v.len())
            .filter(|&k| {
                v.op(k) == Op::TargetIntrinsic
                    && v.intrinsic_name(IntrinsicId(v.extra[v.data[k] as usize]))
                        == PREFETCH_READ_L1_INTRINSIC.as_bytes()
            })
            .count();
        assert_eq!(pf_count, 2);

        // Verify clamped `i + 4` lookahead via `Op::Umin`.
        assert!(
            (0..v.len()).any(|k| v.op(k) == Op::Umin),
            "expected umin clamp bounding lookahead within [0, n)"
        );
        assert!(
            (0..v.len()).any(|k| const_u64(&v, k) == Some(4)),
            "expected lookahead constant 4 in prefetched loop"
        );

        // Idempotence: a second pass must not insert duplicate prefetches.
        let again = insert_software_prefetch(out.func.view(), 4, TestHeap).unwrap();
        assert_eq!(again.stats.loops_prefetched, 0);
        assert_eq!(again.stats.prefetches_inserted, 0);
    }

    fn eval_swisstable_probe(probe: &Func<TestHeap>, ctrl: [u8; 16], h2: u8) -> u64 {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let caller_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8)], &[Param::of(Ty::I64)]);
        let probe_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I8)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(caller_sig);
        let callee_ref = b.reference(RefKind::Func, 0);
        let slot = b.slot(16, 0, SlotKind::Temp);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let h2_arg = b.param(Ty::I8);
        let base = b.slot_addr(slot);
        for (idx, &byte) in ctrl.iter().enumerate() {
            let bv = b.iconst(Ty::I8, u128::from(byte));
            b.store(Flags::NONE, bv, base, idx as i32, MemAttrs::default());
        }
        let mask = b.call(Flags::NONE, callee_ref, probe_sig, &[base, h2_arg]);
        b.ret(&[mask]);
        let harness = b.finish_checked().unwrap();

        let bodies = SingleCallee { func: probe };
        let outcome = run_calls_in(
            harness.view(),
            CALLER_ID,
            &[Val::Int(u128::from(h2))],
            FUEL,
            &bodies,
            TestHeap,
        )
        .expect("evaluator runs swisstable_probe_16b");
        match outcome {
            Outcome::Returned(rets) => match rets[..] {
                [Val::Int(v)] => v as u64,
                _ => panic!("unexpected return values: {rets:?}"),
            },
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn swisstable_probe_16b_matches_exact_bitmask() {
        let probe = build_swisstable_probe_16b(TestHeap).unwrap();
        verify(probe.view()).expect("swisstable_probe_16b must verify");

        let ref_mask = |ctrl: [u8; 16], h2: u8| -> u64 {
            let mut m = 0u64;
            for (k, &c) in ctrl.iter().enumerate() {
                if c == h2 {
                    m |= 1u64 << k;
                }
            }
            m
        };

        let cases: [([u8; 16], u8); 7] = [
            // All empty (0x80) vs probe for empty sentinel and for a full tag.
            ([0x80; 16], 0x80),
            ([0x80; 16], 0x42),
            // Adjacent `h2` and `h2 ^ 1` (catches naive SWAR cross-byte borrow bugs!).
            (
                [
                    0x42, 0x43, 0x42, 0x41, 0x80, 0xFE, 0x00, 0x42,
                    0x43, 0x42, 0x43, 0xFF, 0x7F, 0x42, 0x43, 0x42,
                ],
                0x42,
            ),
            // Boundary lanes: 0, 7, 8, 15.
            (
                [
                    0x19, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x19,
                    0x19, 0x80, 0x81, 0xFE, 0xFF, 0x18, 0x1A, 0x19,
                ],
                0x19,
            ),
            // All 0x00 and all 0xFF extremes.
            ([0x00; 16], 0x00),
            ([0xFF; 16], 0xFF),
            // Alternating pattern.
            (
                [
                    0x5A, 0xA5, 0x5A, 0xA5, 0x5A, 0xA5, 0x5A, 0xA5,
                    0x5A, 0xA5, 0x5A, 0xA5, 0x5A, 0xA5, 0x5A, 0xA5,
                ],
                0xA5,
            ),
        ];

        for (ctrl, h2) in cases {
            let got = eval_swisstable_probe(&probe, ctrl, h2);
            let want = ref_mask(ctrl, h2);
            assert_eq!(got, want, "mismatch for ctrl={ctrl:02x?} h2=0x{h2:02x}");
        }

        // Single-bit match across every lane 0..16.
        for lane in 0..16usize {
            let mut ctrl = [0x80u8; 16];
            ctrl[lane] = 0x37;
            assert_eq!(eval_swisstable_probe(&probe, ctrl, 0x37), 1u64 << lane);
        }
    }

    fn eval_fast_utoa(utoa: &Func<TestHeap>, val: u64, expected: &[u8]) {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let caller_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64)],
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
        );
        let utoa_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::PTR)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(caller_sig);
        let callee_ref = b.reference(RefKind::Func, 0);
        let dst_slot = b.slot(24, 0, SlotKind::Temp);
        let exp_slot = b.slot(24, 0, SlotKind::Temp);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let v = b.param(Ty::I64);
        let dst = b.slot_addr(dst_slot);
        let exp = b.slot_addr(exp_slot);
        for (idx, &byte) in expected.iter().enumerate() {
            let bv = b.iconst(Ty::I8, u128::from(byte));
            b.store(Flags::NONE, bv, exp, idx as i32, MemAttrs::default());
        }
        let written = b.call(Flags::NONE, callee_ref, utoa_sig, &[v, dst]);
        let exp_len = b.iconst(Ty::I64, expected.len() as u128);
        let cmp = b.memcmp(dst, exp, exp_len, MemAttrs::default());
        b.ret(&[written, cmp]);
        let harness = b.finish_checked().unwrap();

        let bodies = SingleCallee { func: utoa };
        let outcome = run_calls_in(
            harness.view(),
            CALLER_ID,
            &[Val::Int(u128::from(val))],
            FUEL,
            &bodies,
            TestHeap,
        )
        .expect("evaluator runs fast_utoa_u64");
        match outcome {
            Outcome::Returned(rets) => {
                assert_eq!(
                    rets[..],
                    [Val::Int(expected.len() as u128), Val::Int(0)],
                    "fast_utoa_u64({val}) expected {:?} (len {})",
                    core::str::from_utf8(expected).unwrap(),
                    expected.len()
                );
            }
            other => panic!("unexpected outcome for fast_utoa_u64({val}): {other:?}"),
        }
    }

    #[test]
    fn fast_utoa_u64_formats_zero_small_and_u64_max() {
        let utoa = build_fast_utoa_u64(TestHeap).unwrap();
        verify(utoa.view()).expect("fast_utoa_u64 must verify");

        eval_fast_utoa(&utoa, 0, b"0");
        eval_fast_utoa(&utoa, 7, b"7");
        eval_fast_utoa(&utoa, 10, b"10");
        eval_fast_utoa(&utoa, 42, b"42");
        eval_fast_utoa(&utoa, 123_456_789, b"123456789");
        eval_fast_utoa(&utoa, u64::MAX, b"18446744073709551615");
    }
}

//! Alias-Aware Memory LICM, Non-Escaping Slot Analysis, and Dead Store Elimination (`memopt`).
//!
//! Eliminates redundant memory traffic across loops and basic blocks:
//! - **Pointer Decomposition & Escape Analysis**: tracks `(base, const_byte_offset)` through
//!   `Op::PtrAdd` chains and identifies non-escaping `Op::SlotAddr` local stack slots.
//! - **Exact Non-Aliasing Rules**: distinct `SlotAddr`s never alias; a non-escaping `SlotAddr`
//!   never aliases any external pointer; same-base non-overlapping `[off, off + size)` byte
//!   intervals never alias.
//! - **Memory LICM**: hoists non-volatile loop-invariant loads out of natural loops into the
//!   dominating `Jump` preheader when no aliasing store/call/atomic occurs in the loop AND the
//!   load is guaranteed dereferenceable (in `header` or from a `SlotAddr`).
//! - **Dead Store Elimination (DSE)**: removes overwritten non-volatile stores to the exact same
//!   `(base, offset, size)` with no intervening aliasing load/call/atomic/fence.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, dead, each, filled, Item},
};
use crate::{
    Block, Flags, Func, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::MemAttrsRec,
    verify,
};

/// Telemetry from [`optimize_memory`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MemOptStats {
    /// Loop-invariant loads physically hoisted into loop preheaders.
    pub loads_hoisted: u32,
    /// Overwritten dead stores eliminated within basic blocks.
    pub dead_stores_eliminated: u32,
}

/// Output of [`optimize_memory`].
#[derive(Debug)]
pub struct MemOptOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: MemOptStats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct MemLoc {
    /// Underlying pointer SSA value (or `SlotAddr` instruction).
    base: u32,
    /// Constant byte offset from `base`.
    off: i64,
    /// Access byte width (`1, 2, 4, 8, 16`).
    size: i64,
    /// Local stack slot index this pointer derives from (`u32::MAX` if not derived from a slot).
    slot: u32,
    /// `true` when `off` is exact relative to `SlotAddr(slot)` (false if derived via dynamic `PtrAdd`).
    off_known: bool,
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
fn trunc_u64(v: u128) -> u64 {
    u64::try_from(v & u128::from(u64::MAX)).unwrap_or(0)
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
fn const_i64(view: &View<'_>, inst: usize) -> Option<i64> {
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
    Some(trunc_u64(v).cast_signed())
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() {
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

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn small_memop_ty(view: &View<'_>, inst: usize) -> Option<Ty> {
    if !matches!(view.op(inst), Op::Memcpy | Op::Memmove) {
        return None;
    }
    if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
        return None;
    }
    let at = to_idx(at_u32(view.data, inst));
    let len_val = at_u32(view.extra, at.checked_add(2)?);
    match const_u64(view, to_idx(len_val))? {
        1 => Some(Ty::I8),
        2 => Some(Ty::I16),
        4 => Some(Ty::I32),
        8 => Some(Ty::I64),
        _ => None,
    }
}

fn intern_align_attr<A: Allocator>(func: &mut Func<A>, align: u8) -> u32 {
    if align == 0 {
        return 0;
    }
    let rec = MemAttrsRec { range: 0, align, src_align: 0, pointee_align: 0, bits: 0 };
    if let Some(pos) = func.mem_attrs.iter().position(|it| *it == rec) {
        return to_u32(pos).saturating_add(1);
    }
    let row = to_u32(func.mem_attrs.len());
    func.mem_attrs.push(rec);
    row.saturating_add(1)
}

/// Scalarizes non-volatile `Op::Memcpy` and `Op::Memmove` instructions with constant length
/// `1`, `2`, `4`, or `8` bytes into an overlap-safe `Op::Load` + `Op::Store` pair (`Ty::I8`,
/// `Ty::I16`, `Ty::I32`, `Ty::I64`).
///
/// Returns `Ok(None)` with zero heap allocations when no eligible instruction exists.
pub fn scalarize_small_memops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    let len = view.len();
    let mut has_small = false;
    for inst in 0..len {
        if small_memop_ty(&view, inst).is_some() {
            has_small = true;
            break;
        }
    }
    if !has_small {
        return Ok(None);
    }

    // Pass 1: precompute the exact old->new instruction index map in layout order so any
    // non-RPO forward value reference resolves in O(1) without a fixup pass.
    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    for inst in 0..len {
        if small_memop_ty(&view, inst).is_some() {
            // `Memcpy`/`Memmove` produces no value (`Ty::NONE`) and expands to `Load` + `Store`.
            next_idx = next_idx.saturating_add(2);
        } else {
            set_at(&mut val_map, inst, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
    }
    if next_idx >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers" });
    }

    // Pass 2: rebuild instructions, replacing each matching bulk op with `Load(ty, src)` +
    // `Store(val, dst)`. Because the full `k`-byte load finishes into an SSA value before the
    // store writes a single byte, this is overlap-safe even for `Op::Memmove`.
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

    let load_op = op_byte(Op::Load);
    let store_op = op_byte(Op::Store);
    let blocks = view.blocks();
    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            if let Some(ty) = small_memop_ty(&view, inst) {
                let at = to_idx(at_u32(view.data, inst));
                let old_dst = at_u32(view.extra, at);
                let old_src = at_u32(view.extra, at.saturating_add(1));
                let new_dst = at_u32(&val_map, to_idx(old_dst));
                let new_src = at_u32(&val_map, to_idx(old_src));
                let loc = at_u32(view.locs, inst);
                let (dst_align, src_align) = match view.mem_attrs(inst) {
                    Some(m) => (m.align, m.src_align),
                    None => (0, 0),
                };
                let load_attr = intern_align_attr(&mut func, src_align);
                let store_attr = intern_align_attr(&mut func, dst_align);

                let load_extra = to_u32(func.extra.len());
                func.extra.push(new_src);
                func.extra.push(0);
                let load_idx = to_u32(func.ops.len());
                func.ops.push(load_op);
                func.flags.push(Flags::NONE.0);
                func.tys.push(ty.0);
                func.data.push(load_extra);
                func.attrs.push(load_attr);
                func.locs.push(loc);

                let store_extra = to_u32(func.extra.len());
                func.extra.push(load_idx);
                func.extra.push(new_dst);
                func.extra.push(0);
                func.ops.push(store_op);
                func.flags.push(Flags::NONE.0);
                func.tys.push(Ty::NONE.0);
                func.data.push(store_extra);
                func.attrs.push(store_attr);
                func.locs.push(loc);
            } else {
                let _ = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(Some(func))
}

#[inline]
fn access_bytes(ty: Ty, ptr_bits: u32) -> Option<i64> {
    if ty.is_ptr() {
        Some(i64::from(ptr_bits / 8))
    } else if ty.is_vector() {
        Some(i64::from(ty.elem().bits(ptr_bits) * ty.lanes() / 8))
    } else if ty.is_scalar() {
        let b = ty.bits(ptr_bits);
        if matches!(b, 8 | 16 | 32 | 64 | 128) { Some(i64::from(b / 8)) } else { None }
    } else {
        None
    }
}

/// Decomposes `(ptr, extra_off, access_ty)` through up to 8 `Op::PtrAdd(_, const)` hops.
fn decompose_loc(view: &View<'_>, slot_of: &[u32], mut ptr: u32, mut off: i64, ty: Ty) -> Option<MemLoc> {
    let size = access_bytes(ty, view.head.ptr_bits)?;
    for _ in 0..8 {
        let pi = to_idx(ptr);
        if view.op(pi) == Op::PtrAdd {
            let at = to_idx(at_u32(view.data, pi));
            let base = at_u32(view.extra, at);
            let by = at_u32(view.extra, at + 1);
            if let Some(d) = const_i64(view, to_idx(by)) {
                if let Some(next_off) = off.checked_add(d) {
                    ptr = base;
                    off = next_off;
                    continue;
                }
            }
        }
        break;
    }
    let pi = to_idx(ptr);
    let (slot, off_known) = if view.op(pi) == Op::SlotAddr {
        (at_u32(view.data, pi), true)
    } else {
        let derived = at_u32(slot_of, pi);
        if derived != u32::MAX { (derived, false) } else { (u32::MAX, true) }
    };
    Some(MemLoc { base: ptr, off, size, slot, off_known })
}

/// Computes which local stack slots never have their address escape (only used as direct pointer
/// operand of `Load`/`Store`/`PtrAdd` chains), returning `(non_escaping, slot_of)`.
fn compute_non_escaping_slots<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: &A,
) -> (Vec<bool, A>, Vec<u32, A>) {
    let nslots = view.slots.len();
    let mut non_escaping = filled(nslots, true, alloc);
    let len = view.len();
    // Propagate which SSA values derive from a slot (`slot_of[v] = slot_id` or `u32::MAX`).
    let mut slot_of = filled(len, u32::MAX, alloc);
    for inst in 0..len {
        let op = view.op(inst);
        if op == Op::SlotAddr {
            set_at(&mut slot_of, inst, at_u32(view.data, inst));
        } else if op == Op::PtrAdd {
            let base = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
            let base_slot = at_u32(&slot_of, base);
            set_at(&mut slot_of, inst, base_slot);
        }
    }
    for inst in 0..len {
        let op = view.op(inst);
        match op {
            Op::Load => {
                // Pointer operand does not escape.
            }
            Op::Store => {
                // Stored VALUE escapes if it holds a slot address; pointer operand does not.
                let val = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
                let s = at_u32(&slot_of, val);
                if s != u32::MAX {
                    set_at(&mut non_escaping, to_idx(s), false);
                }
            }
            Op::PtrAdd => {
                // Base propagates via `slot_of`; index cannot be a pointer.
            }
            Op::SlotLive | Op::SlotDead => {}
            _ => {
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        let s = at_u32(&slot_of, to_idx(v));
                        if s != u32::MAX {
                            set_at(&mut non_escaping, to_idx(s), false);
                        }
                    }
                });
            }
        }
    }
    (non_escaping, slot_of)
}

/// Returns `true` if `a` and `b` are provably disjoint (`NoAlias`).
#[inline]
fn no_alias(a: MemLoc, b: MemLoc, non_escaping: &[bool]) -> bool {
    if a.slot != u32::MAX && b.slot != u32::MAX && a.slot != b.slot {
        return true;
    }
    if a.slot != u32::MAX && at_bool(non_escaping, to_idx(a.slot)) && b.slot != a.slot {
        return true;
    }
    if b.slot != u32::MAX && at_bool(non_escaping, to_idx(b.slot)) && a.slot != b.slot {
        return true;
    }
    if a.base == b.base || (a.slot != u32::MAX && a.slot == b.slot && a.off_known && b.off_known) {
        let a_end = a.off.saturating_add(a.size);
        let b_end = b.off.saturating_add(b.size);
        return a_end <= b.off || b_end <= a.off;
    }
    false
}

/// Runs alias-aware Memory LICM and intra-block Dead Store Elimination on `view`.
pub fn optimize_memory<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<MemOptOutput<A>, SimplifyError> {
    let mut stats = MemOptStats::default();
    let (after_licm, hoisted) = hoist_invariant_loads(view, alloc)?;
    stats.loads_hoisted = hoisted;
    let (after_dse, dse_count) = eliminate_dead_stores(after_licm.view(), alloc)?;
    stats.dead_stores_eliminated = dse_count;
    if hoisted == 0 && dse_count == 0 {
        return Ok(MemOptOutput { func: after_dse, stats });
    }
    let cleaned = dead(after_dse.view(), alloc)?;
    Ok(MemOptOutput { func: cleaned, stats })
}

fn hoist_invariant_loads<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return Ok((view.to_func_in(alloc), 0));
    }

    let (non_escaping, slot_of) = compute_non_escaping_slots(&view, &alloc);
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
        each(&view, term, |item| {
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

    let mut hoist_to = filled(len, NONE, &alloc);
    let mut hoisted_by_pre: Vec<Vec<usize, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        hoisted_by_pre.push(Vec::new_in(alloc));
    }
    let mut total_hoisted = 0u32;
    let mut in_loop = filled(blocks, false, &alloc);

    for h in 1..blocks {
        let header = to_u32(h);
        // Collect latches (`p >= header` jumping back to `header`).
        let mut latches: Vec<u32, A> = Vec::new_in(alloc);
        let mut outside_preds = 0u32;
        let mut preheader = NONE;
        if let Some(h_preds) = preds.get(h) {
            for &p in h_preds {
                if p >= header {
                    latches.push(p);
                } else {
                    outside_preds += 1;
                    preheader = p;
                }
            }
        }
        if latches.is_empty() || outside_preds != 1 || preheader == NONE {
            continue;
        }
        if view.op(view.terminator(Block(preheader))) != Op::Jump {
            continue;
        }

        // Backward walk from latches to `header` to collect loop blocks.
        let mut body: Vec<u32, A> = Vec::new_in(alloc);
        let mut work: Vec<u32, A> = Vec::new_in(alloc);
        set_at(&mut in_loop, h, true);
        body.push(header);
        for &l in &latches {
            let li = to_idx(l);
            if !at_bool(&in_loop, li) {
                set_at(&mut in_loop, li, true);
                body.push(l);
                work.push(l);
            }
        }
        let mut malformed = false;
        while let Some(cur) = work.pop() {
            if let Some(c_preds) = preds.get(to_idx(cur)) {
                for &p in c_preds {
                    if p < header {
                        malformed = true;
                        break;
                    }
                    let pi = to_idx(p);
                    if !at_bool(&in_loop, pi) {
                        set_at(&mut in_loop, pi, true);
                        body.push(p);
                        work.push(p);
                    }
                }
            }
            if malformed {
                break;
            }
        }
        if malformed {
            for &bb in &body {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        }

        // Collect all memory clobbers inside the loop body.
        let mut has_opaque_clobber = false;
        let mut has_unknown_store = false;
        let mut store_locs: Vec<MemLoc, A> = Vec::new_in(alloc);
        for &bb in &body {
            for inst in view.block(Block(bb)) {
                let op = view.op(inst);
                match op {
                    Op::Store => {
                        if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
                            has_unknown_store = true;
                        } else {
                            let at = to_idx(at_u32(view.data, inst));
                            let val_ty = Ty(at_u16(view.tys, to_idx(at_u32(view.extra, at))));
                            let ptr = at_u32(view.extra, at + 1);
                            let off = i64::from(at_u32(view.extra, at + 2).cast_signed());
                            if let Some(loc) = decompose_loc(&view, &slot_of, ptr, off, val_ty) {
                                store_locs.push(loc);
                            } else {
                                has_unknown_store = true;
                            }
                        }
                    }
                    Op::MaskedStore | Op::Scatter | Op::SlotDead => {
                        has_unknown_store = true;
                    }
                    Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::TailCall
                    | Op::TailCallIndirect
                    | Op::Memcpy
                    | Op::Memmove
                    | Op::Memset
                    | Op::AtomicStore
                    | Op::AtomicRmw
                    | Op::Cmpxchg
                    | Op::Fence
                    | Op::Asm
                    | Op::AsmBr
                    | Op::TargetIntrinsic => {
                        has_opaque_clobber = true;
                    }
                    _ => {}
                }
            }
        }
        if has_unknown_store {
            for &bb in &body {
                set_at(&mut in_loop, to_idx(bb), false);
            }
            continue;
        }

        // Hoist non-volatile invariant loads that are guaranteed dereferenceable (in `header` or from a `SlotAddr`).
        body.sort_unstable();
        for &bb in &body {
            for inst in view.block(Block(bb)) {
                if at_u32(&hoist_to, inst) != NONE || view.op(inst) != Op::Load {
                    continue;
                }
                if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
                    continue;
                }
                let at = to_idx(at_u32(view.data, inst));
                let ptr = at_u32(view.extra, at);
                let off = i64::from(at_u32(view.extra, at + 1).cast_signed());
                let ptr_b = at_u32(&inst_block, to_idx(ptr));
                // Pointer must be defined outside the loop and before/at `preheader`!
                if at_bool(&in_loop, to_idx(ptr_b)) || ptr_b > preheader {
                    continue;
                }
                let Some(loc) = decompose_loc(&view, &slot_of, ptr, off, Ty(at_u16(view.tys, inst))) else {
                    continue;
                };
                // Crash safety: only hoist if in `header` (always runs on entry) OR local `SlotAddr` (always valid memory)!
                let guaranteed_deref = bb == header || (loc.slot != u32::MAX && loc.off_known);
                if !guaranteed_deref {
                    continue;
                }
                // Check clobbers: opaque calls/invokes/fences only spare non-escaping local slots!
                if has_opaque_clobber && !(loc.slot != u32::MAX && at_bool(&non_escaping, to_idx(loc.slot))) {
                    continue;
                }
                let aliased = store_locs.iter().any(|&st| !no_alias(loc, st, &non_escaping));
                if aliased {
                    continue;
                }
                set_at(&mut hoist_to, inst, preheader);
                if let Some(hp) = hoisted_by_pre.get_mut(to_idx(preheader)) {
                    hp.push(inst);
                }
                total_hoisted += 1;
            }
        }

        for &bb in &body {
            set_at(&mut in_loop, to_idx(bb), false);
        }
    }

    if total_hoisted == 0 {
        return Ok((view.to_func_in(alloc), 0));
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
    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let term = range.end - 1;
        for inst in range.start..term {
            if at_u32(&hoist_to, inst) != NONE {
                continue;
            }
            let mapped = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
            set_at(&mut val_map, inst, mapped);
        }
        if let Some(hb) = hoisted_by_pre.get(b) {
            for &hi in hb {
                let mapped = copy_inst_same_blocks(&view, hi, &val_map, &mut func);
                set_at(&mut val_map, hi, mapped);
            }
        }
        let mapped = copy_inst_same_blocks(&view, term, &val_map, &mut func);
        set_at(&mut val_map, term, mapped);
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok((func, total_hoisted))
}

fn eliminate_dead_stores<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    let (non_escaping, slot_of) = compute_non_escaping_slots(&view, &alloc);
    let mut kill = filled(len, false, &alloc);
    let mut killed = 0u32;

    let mut killed_locs: Vec<MemLoc, A> = Vec::new_in(alloc);
    for b in 0..blocks {
        killed_locs.clear();
        let range = view.block(Block(to_u32(b)));
        for inst in (range.start..range.end).rev() {
            let op = view.op(inst);
            match op {
                Op::Store => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val_ty = Ty(at_u16(view.tys, to_idx(at_u32(view.extra, at))));
                    let ptr = at_u32(view.extra, at + 1);
                    let off = i64::from(at_u32(view.extra, at + 2).cast_signed());
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    if volatile {
                        killed_locs.clear();
                    } else if let Some(loc) = decompose_loc(&view, &slot_of, ptr, off, val_ty) {
                        if killed_locs.iter().any(|&k| {
                            k.off == loc.off
                                && k.size == loc.size
                                && (k.base == loc.base
                                    || (k.slot != u32::MAX && k.slot == loc.slot && k.off_known && loc.off_known))
                        }) {
                            set_at(&mut kill, inst, true);
                            killed += 1;
                        } else if killed_locs.len() < 16 {
                            killed_locs.push(loc);
                        }
                    } else {
                        killed_locs.clear();
                    }
                }
                Op::Load => {
                    let at = to_idx(at_u32(view.data, inst));
                    let ptr = at_u32(view.extra, at);
                    let off = i64::from(at_u32(view.extra, at + 1).cast_signed());
                    if let Some(loc) = decompose_loc(&view, &slot_of, ptr, off, Ty(at_u16(view.tys, inst))) {
                        let mut i = 0;
                        while i < killed_locs.len() {
                            if let Some(&k) = killed_locs.get(i) {
                                if !no_alias(loc, k, &non_escaping) {
                                    killed_locs.remove_swap(i);
                                    continue;
                                }
                            }
                            i += 1;
                        }
                    } else {
                        killed_locs.clear();
                    }
                }
                Op::MaskedLoad | Op::Gather | Op::MaskedStore | Op::Scatter | Op::SlotDead => {
                    killed_locs.clear();
                }
                Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::TailCall
                | Op::TailCallIndirect
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::Memcmp
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::Asm
                | Op::AsmBr
                | Op::TargetIntrinsic
                | Op::Resume
                | Op::Trap
                | Op::Unreachable
                | Op::Cancelled => {
                    // Only non-escaping local stack slots survive calls/invokes/fences.
                    let mut i = 0;
                    while i < killed_locs.len() {
                        if let Some(&k) = killed_locs.get(i) {
                            if k.slot == u32::MAX || !at_bool(&non_escaping, to_idx(k.slot)) {
                                killed_locs.remove_swap(i);
                                continue;
                            }
                        }
                        i += 1;
                    }
                }
                _ => {}
            }
        }
    }

    if killed == 0 {
        return Ok((view.to_func_in(alloc), 0));
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
    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            if at_bool(&kill, inst) {
                continue;
            }
            let mapped = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
            set_at(&mut val_map, inst, mapped);
        }
    }
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok((func, killed))
}

fn copy_inst_same_blocks<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    use crate::op::{Field, Layout};
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

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, SlotKind},
    };

    #[test]
    fn memory_licm_hoists_non_aliasing_invariant_load_out_of_loop() {
        // Loop writes to `[dst + 8]` while reading invariant `[dst + 0]` in header: same base, non-overlapping!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let scale = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let prod = b.binary(Op::Mul, Flags::NONE, i, scale);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, prod);
        b.store(Flags::NONE, acc2, ptr, 8, MemAttrs::aligned(8));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(crate::IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (header, &[i2, acc2]), (exit, &[acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loads_hoisted, 1, "non-overlapping invariant load at [ptr+0] must hoist to preheader");
    }

    #[test]
    fn dead_store_elimination_removes_overwritten_store_on_non_aliasing_slot() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let ext = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let s = b.slot(16, 3, SlotKind::Named);
        let sp = b.slot_addr(s);
        let k1 = b.iconst(Ty::I64, 111);
        let k2 = b.iconst(Ty::I64, 222);
        b.store(Flags::NONE, k1, sp, 0, MemAttrs::aligned(8)); // dead!
        let ext_v = b.load(Ty::I64, Flags::NONE, ext, 0, MemAttrs::aligned(8)); // cannot alias non-escaping slot `s`!
        let sum = b.binary(Op::Add, Flags::NONE, x, ext_v);
        b.store(Flags::NONE, k2, sp, 0, MemAttrs::aligned(8)); // overwrites `[sp + 0]`!
        let loaded = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(8));
        let res = b.binary(Op::Add, Flags::NONE, sum, loaded);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.dead_stores_eliminated, 1, "overwritten store across non-aliasing ext load must be eliminated");
    }

    #[test]
    fn memory_licm_does_not_hoist_sret_slot_load_past_invoke() {
        // Exact `Iterator::fold` pattern: `SlotAddr` in entry, loop invokes `next(sret = slot)`
        // and then loads the discriminant from `slot`. `Op::Invoke` clobbers escaping `slot`, so
        // the load MUST NOT hoist above the loop!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        let callee = b.reference(crate::func::RefKind::Func, 1);
        let [entry, header, after_call, exit, lpad] = [
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Cleanup),
        ];

        b.begin(entry);
        let iter_ptr = b.param(Ty::PTR);
        let ret_slot = b.slot(48, 3, SlotKind::Named);
        let ret_ptr = b.slot_addr(ret_slot);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));

        b.begin(header);
        let acc = b.param(Ty::I64);
        b.invoke(Flags::NONE, callee, callee_sig, &[iter_ptr, ret_ptr], after_call, lpad);

        b.begin(after_call);
        let disc = b.load(Ty::I8, Flags::NONE, ret_ptr, 0, MemAttrs::aligned(1));
        let zero_i8 = b.iconst(Ty::I8, 0);
        let is_some = b.icmp(crate::IntCond::Ne, disc, zero_i8);
        let payload = b.load(Ty::I64, Flags::NONE, ret_ptr, 8, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, payload);
        b.br(Flags::NONE, is_some, (header, &[acc2]), (exit, &[acc]));

        b.begin(exit);
        let out_val = b.param(Ty::I64);
        b.ret(&[out_val]);

        b.begin(lpad);
        let exc = b.param(Ty::PTR);
        b.resume(exc);

        let func = b.finish_checked().unwrap();
        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(
            out.stats.loads_hoisted, 0,
            "sret slot load after Op::Invoke inside loop must NEVER hoist into preheader"
        );
    }

    #[test]
    fn memory_licm_does_not_hoist_slot_load_past_dynamic_ptradd_store() {
        // Non-escaping local slot read at `[s + 0]` and written via `PtrAdd(s, i)` in loop:
        // dynamic index MAY alias offset 0, so the load must NOT hoist!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let s = b.slot(64, 3, SlotKind::Named);
        let sp = b.slot_addr(s);
        let zero = b.iconst(Ty::I64, 0);
        b.store(Flags::NONE, zero, sp, 0, MemAttrs::aligned(8));
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cur = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, cur);
        let elem_ptr = b.ptr_add(Flags::NONE, sp, i);
        b.store(Flags::NONE, acc2, elem_ptr, 0, MemAttrs::aligned(1));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(crate::IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (header, &[i2, acc2]), (exit, &[acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();
        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(
            out.stats.loads_hoisted, 0,
            "load from [slot+0] must NOT hoist across dynamic PtrAdd(slot, i) store"
        );
    }

    #[test]
    fn scalarize_small_memops_rewrites_swap_and_enables_sroa() {
        use crate::{
            eval::{Bodies, BodyId, Outcome, Val, run_calls_in, run_in},
            func::{RefId, RefKind},
        };

        let alloc = blaze_own::testing::heap().alloc();

        // Build a `slice::swap`-style function using a local 8-byte `StackSlot` and 3
        // `Op::Memmove(..., 8)` calls (`tmp <- p`, `p <- q`, `q <- tmp`).
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let swap_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        b.set_signature(swap_sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let p = b.param(Ty::PTR);
        let q = b.param(Ty::PTR);
        let tmp = b.slot(8, 3, SlotKind::Temp);
        let tp = b.slot_addr(tmp);
        let eight = b.iconst(Ty::I64, 8);
        let aligned8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
        let dst_only = MemAttrs::aligned(3);
        b.bulk(Op::Memmove, Flags::NONE, tp, p, eight, dst_only);
        b.bulk(Op::Memmove, Flags::NONE, p, q, eight, aligned8);
        b.bulk(Op::Memmove, Flags::NONE, q, tp, eight, aligned8);
        b.ret(&[]);
        let swap_fn = b.finish_checked().expect("valid swap input");

        // Run `scalarize_small_memops` -> `scalar_replace` -> `aegraph_optimize`.
        let scalarized = scalarize_small_memops(swap_fn.view(), alloc)
            .expect("scalarize_small_memops succeeds")
            .expect("must scalarize 8-byte memmove");
        // First `Memmove` had `src_align == 0`: emitted `Load` must not over-claim alignment.
        let first_load = (0..scalarized.view().len())
            .find(|&i| scalarized.view().op(i) == Op::Load)
            .expect("must emit Load");
        assert_eq!(scalarized.view().attrs.get(first_load).copied(), Some(0));

        let after_sroa = super::super::scalar_replace(scalarized.view(), alloc).expect("sroa succeeds");
        let after_ae = super::super::egraph::aegraph_optimize(after_sroa.view(), alloc)
            .expect("aegraph succeeds")
            .func;
        verify(after_ae.view()).expect("optimized swap verifies");
        assert!(after_ae.slots.is_empty(), "swap temp stack slot must be eliminated by SROA");
        for inst in 0..after_ae.view().len() {
            let op = after_ae.view().op(inst);
            assert_ne!(op, Op::Memmove, "no Op::Memmove may remain after scalarize + sroa + aegraph");
            assert_ne!(op, Op::SlotAddr, "no Op::SlotAddr may remain after scalarize + sroa + aegraph");
        }

        // Caller `driver(a0, a1, same)` swaps `arr[0]` and `arr[same ? 0 : 1]` via `swap(p, q)`,
        // exercising both distinct (`i != j`) and overlapping (`i == j`) pointers.
        let mut db = Builder::<_, Full>::new_in(64, alloc);
        let drv_sig = db.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I1)],
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
        );
        db.set_signature(drv_sig);
        let callee_sig = db.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        let callee = db.reference(RefKind::Func, 0);
        let d_entry = db.create_block(BlockKind::Normal);
        db.begin(d_entry);
        let a0 = db.param(Ty::I64);
        let a1 = db.param(Ty::I64);
        let same = db.param(Ty::I1);
        let arr = db.slot(16, 3, SlotKind::Named);
        let base = db.slot_addr(arr);
        db.store(Flags::NONE, a0, base, 0, MemAttrs::aligned(3));
        db.store(Flags::NONE, a1, base, 8, MemAttrs::aligned(3));
        let off8 = db.iconst(Ty::I64, 8);
        let p1 = db.ptr_add(Flags::NONE, base, off8);
        let q_ptr = db.select(Flags::NONE, same, base, p1);
        let _ = db.call(Flags::NONE, callee, callee_sig, &[base, q_ptr]);
        let r0 = db.load(Ty::I64, Flags::NONE, base, 0, MemAttrs::aligned(3));
        let r1 = db.load(Ty::I64, Flags::NONE, base, 8, MemAttrs::aligned(3));
        db.ret(&[r0, r1]);
        let driver = db.finish_checked().expect("valid driver");

        struct OneCallee<'a> {
            view: View<'a>,
        }
        impl Bodies for OneCallee<'_> {
            fn callee(&self, _from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
                match reference {
                    RefId(0) => Some((BodyId([2; 32]), self.view)),
                    _ => None,
                }
            }
        }

        let root = BodyId([1; 32]);
        let fuel = 10_000;
        let x: u128 = 0x1111_2222_3333_4444;
        let y: u128 = 0xAAAA_BBBB_CCCC_DDDD;
        for (same_flag, want0, want1) in [(0u128, y, x), (1u128, x, y)] {
            let args = [Val::Int(x), Val::Int(y), Val::Int(same_flag)];
            let before =
                run_calls_in(driver.view(), root, &args, fuel, &OneCallee { view: swap_fn.view() }, alloc)
                    .expect("eval unoptimized swap");
            let after =
                run_calls_in(driver.view(), root, &args, fuel, &OneCallee { view: after_ae.view() }, alloc)
                    .expect("eval optimized swap");
            assert_eq!(before, after, "swap mismatch for same={same_flag}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want0), Val::Int(want1)]),
                "unexpected swap result for same={same_flag}: {after:?}"
            );
        }

        // Also test direct partial-overlap `Op::Memmove(buf + 4, buf + 0, 8)` via `eval::run_in`.
        let mut ob = Builder::<_, Full>::new_in(64, alloc);
        let ov_sig = ob.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        ob.set_signature(ov_sig);
        let o_entry = ob.create_block(BlockKind::Normal);
        ob.begin(o_entry);
        let val = ob.param(Ty::I64);
        let buf = ob.slot(12, 2, SlotKind::Named);
        let bp = ob.slot_addr(buf);
        ob.store(Flags::NONE, val, bp, 0, MemAttrs::aligned(2));
        let off4 = ob.iconst(Ty::I64, 4);
        let dst4 = ob.ptr_add(Flags::NONE, bp, off4);
        let len8 = ob.iconst(Ty::I64, 8);
        let al4 = MemAttrs { align: 2, src_align: 2, pointee_align: 0, bits: 0, range: None };
        ob.bulk(Op::Memmove, Flags::NONE, dst4, bp, len8, al4);
        let moved = ob.load(Ty::I64, Flags::NONE, dst4, 0, MemAttrs::aligned(2));
        ob.ret(&[moved]);
        let ov_fn = ob.finish_checked().expect("valid overlap fn");
        let ov_opt = scalarize_small_memops(ov_fn.view(), alloc)
            .expect("scalarize overlap succeeds")
            .expect("must scalarize overlap memmove");
        let ov_before = run_in(ov_fn.view(), &[Val::Int(x)], fuel, alloc).expect("eval overlap before");
        let ov_after = run_in(ov_opt.view(), &[Val::Int(x)], fuel, alloc).expect("eval overlap after");
        assert_eq!(ov_before, ov_after);
        assert!(matches!(ov_after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(x)]));
    }

    #[test]
    fn scalarize_small_memops_skips_volatile_and_large_sizes() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let len8 = b.iconst(Ty::I64, 8);
        let len16 = b.iconst(Ty::I64, 16);
        let len3 = b.iconst(Ty::I64, 3);
        let attrs = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
        b.bulk(Op::Memmove, Flags::VOLATILE, dst, src, len8, attrs);
        b.bulk(Op::Memmove, Flags::NONE, dst, src, len16, attrs);
        b.bulk(Op::Memcpy, Flags::VOLATILE, dst, src, len8, attrs);
        b.bulk(Op::Memcpy, Flags::NONE, dst, src, len3, attrs);
        b.ret(&[]);
        let func = b.finish_checked().expect("valid input");

        let out = scalarize_small_memops(func.view(), alloc).expect("pass succeeds");
        assert!(out.is_none(), "volatile 8B and non-1/2/4/8B memops must return Ok(None)");
    }
}

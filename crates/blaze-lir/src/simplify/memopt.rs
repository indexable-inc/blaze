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
    /// If `base` is an `Op::SlotAddr`, its slot index (`u32::MAX` otherwise).
    slot: u32,
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
fn access_bytes(ty: Ty, ptr_bits: u32) -> Option<i64> {
    if ty.is_ptr() {
        Some(i64::from(ptr_bits / 8))
    } else if ty.is_vector() {
        Some(i64::from(ty.elem().bits(ptr_bits) * ty.lanes() / 8))
    } else if ty.is_scalar() {
        let b = ty.bits(ptr_bits);
        if matches!(b, 8 | 16 | 32 | 64) { Some(i64::from(b / 8)) } else { None }
    } else {
        None
    }
}

/// Decomposes `(ptr, extra_off, access_ty)` through up to 8 `Op::PtrAdd(_, const)` hops.
fn decompose_loc(view: &View<'_>, mut ptr: u32, mut off: i64, ty: Ty) -> Option<MemLoc> {
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
    let slot = if view.op(pi) == Op::SlotAddr {
        at_u32(view.data, pi)
    } else {
        u32::MAX
    };
    Some(MemLoc { base: ptr, off, size, slot })
}

/// Computes which local stack slots never have their address escape (only used as direct pointer
/// operand of `Load`/`Store`/`PtrAdd` chains).
fn compute_non_escaping_slots<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Vec<bool, A> {
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
    non_escaping
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
    if a.base == b.base || (a.slot != u32::MAX && a.slot == b.slot) {
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

    let non_escaping = compute_non_escaping_slots(&view, &alloc);
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
        let mut store_locs: Vec<MemLoc, A> = Vec::new_in(alloc);
        for &bb in &body {
            for inst in view.block(Block(bb)) {
                let op = view.op(inst);
                match op {
                    Op::Store => {
                        let at = to_idx(at_u32(view.data, inst));
                        let val_ty = Ty(at_u16(view.tys, to_idx(at_u32(view.extra, at))));
                        let ptr = at_u32(view.extra, at + 1);
                        let off = i64::from(at_u32(view.extra, at + 2).cast_signed());
                        if let Some(loc) = decompose_loc(&view, ptr, off, val_ty) {
                            store_locs.push(loc);
                        } else {
                            has_opaque_clobber = true;
                        }
                    }
                    Op::Call
                    | Op::CallIndirect
                    | Op::Memcpy
                    | Op::Memmove
                    | Op::Memset
                    | Op::AtomicStore
                    | Op::AtomicRmw
                    | Op::Cmpxchg
                    | Op::Fence => {
                        has_opaque_clobber = true;
                    }
                    _ => {}
                }
            }
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
                let Some(loc) = decompose_loc(&view, ptr, off, Ty(at_u16(view.tys, inst))) else {
                    continue;
                };
                // Crash safety: only hoist if in `header` (always runs on entry) OR local `SlotAddr` (always valid memory)!
                let guaranteed_deref = bb == header || loc.slot != u32::MAX;
                if !guaranteed_deref {
                    continue;
                }
                // Check clobbers: opaque calls/fences only spare non-escaping local slots!
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
    let non_escaping = compute_non_escaping_slots(&view, &alloc);
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
                    if let Some(loc) = decompose_loc(&view, ptr, off, val_ty) {
                        if !volatile
                            && killed_locs
                                .iter()
                                .any(|&k| k.base == loc.base && k.off == loc.off && k.size == loc.size)
                        {
                            set_at(&mut kill, inst, true);
                            killed += 1;
                        } else {
                            // Remove any previously recorded store that might alias differently, then record this one.
                            if killed_locs.len() < 16 {
                                killed_locs.push(loc);
                            }
                        }
                    } else {
                        killed_locs.clear();
                    }
                }
                Op::Load => {
                    let at = to_idx(at_u32(view.data, inst));
                    let ptr = at_u32(view.extra, at);
                    let off = i64::from(at_u32(view.extra, at + 1).cast_signed());
                    if let Some(loc) = decompose_loc(&view, ptr, off, Ty(at_u16(view.tys, inst))) {
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
                Op::Call
                | Op::CallIndirect
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::Memcmp
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence => {
                    // Only non-escaping local stack slots survive calls/fences.
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
}

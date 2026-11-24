//! The split of a coroutine body into a state laid out BEFORE the LIR (ASYNC.md P8, option (b),
//! and main's rulings on what crosses a suspension, 2026-11-16, async-P8-4).
//!
//! The state's size, alignment, tag place, and the offset of each saved local of the flow are
//! the caller's ([`Region`], from blaze-build coroutine_state.rs `lay_state` over the flow
//! layer's saved locals' types). The flow layer is the ONE owner of which locals cross a
//! suspension; the pass decides no size and no field. It places:
//! - each slot the builder NAMED as a saved local's ([`SlotName`]: the builder looked the local
//!   up in the flow's table of identities) at that local's offset, kept in the state;
//! - each other slot on the poll function's stack, when no code after a suspension reaches its
//!   bytes ([`reached_after`]: no block on a resume or cancel path, or past an unwind edge from
//!   one, takes its address). A slot the pass's own rule would keep across a suspend
//!   (`kept_slots`: its address taken and its storage not ended) that code after a suspension
//!   DOES reach and that no name places is [`super::CoroutineError::UnnamedAcrossSuspend`]: the
//!   builder made a slot the flow does not save, refused by name (the builder maps the slot to
//!   the expression that made it);
//! - no value: a value live across a suspend is kept ONLY when it is made again after the resume
//!   (an address of a kept slot, a constant: `REMADE`); any other is
//!   [`super::CoroutineError::ValueAcrossSuspend`] (the builder writes each waiting value into
//!   its saved local's slot before the suspend: blaze-build walk/await_spill.rs);
//! - the tag at the region's `tag_offset`, of its `tag_ty`, which must be the width the pass
//!   numbers its points in ([`super::CoroutineError::RegionTag`] otherwise).
//!
//! Sound because a slot's bytes are reached only through an address its `slot_addr` made
//! (coroutine_overlap.rs's module doc): a slot no block after any suspension addresses, and
//! whose address never leaves its own loads and stores (an escaped address counts as reached:
//! it may be read back from memory or a callee after the resume), is never read after a
//! suspension, so its bytes need not outlive the poll that wrote them.

use alloc::vec::Vec;
use core::alloc::Allocator;

use super::{Body, CoroutineError, Panics, Plant, Shape, Split, View, each_value, filled, successors};
use crate::func::{NONE, SlotId};
use crate::{Block, Op, Ty};

/// The state as the caller laid it out from types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    /// The first byte past the upvars: no named slot starts before it.
    pub start: u32,
    /// One past the last byte of the saved fields and drop flags (the tag follows).
    pub end: u32,
    pub tag_offset: u32,
    /// `i8`, `i16` or `i32`.
    pub tag_ty: Ty,
    pub size: u32,
    /// The log2 of the state's alignment.
    pub align: u8,
}

/// A slot of the body that holds a saved local of the flow (or its drop flag), at `offset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotName {
    pub slot: SlotId,
    pub offset: u32,
}

/// [`super::split_with`] into the state `region` lays out, each slot `names` names at its offset.
pub fn split_in<A: Allocator + Clone>(
    view: View<'_>,
    shape: Shape,
    panics: Panics,
    region: Region,
    names: &[SlotName],
    alloc: A,
) -> Result<Split<A>, CoroutineError> {
    super::split_inner(view, shape, panics, Plant::None, Some((region, names)), alloc)
}

/// Per slot, whether bytes written before some suspension may be read after a suspension (on a
/// resume or cancel path, or past an unwind edge from one), or whose address escapes anywhere.
///
/// An unescaped slot (one whose `slot_addr` never leaves its own loads, stores, and bulk
/// memory ops) is reached after a suspension only if a path from some suspend's resume or
/// cancel target reaches a read of the slot before any full-slot overwrite or `slot_dead`.
/// Thus a non-dropping loop local written and read only before `.await` — and overwritten on
/// the next iteration after resume — is not falsely treated as live across the suspension.
pub(super) fn reached_after<A: Allocator + Copy>(body: &Body<'_, A>, alloc: &A) -> Vec<bool, A> {
    let view = &body.view;
    let blocks = view.blocks();
    let slots = view.slots.len();
    let held = |b: usize| body.in_poll[b] || body.in_drop[b];
    let mut out = filled(slots, false, alloc);
    if slots == 0 || body.suspends.is_empty() {
        return out;
    }
    // An address that leaves its slot's own loads and stores anywhere in the body (stored to
    // memory, passed to a call, made into a derived pointer) may be read after a suspension
    // through another place, which no `slot_addr` after it shows: such a slot counts as
    // reached. A `load` through it (operand 0), a `store` through it (operand 1; operand 0 is
    // the value stored), `memcpy`/`memmove`/`memcmp` through it (operands 0 and 1), and
    // `memset` through it (operand 0) only read or write the slot's bytes and keep it
    // unescaped.
    for b in (0..blocks).filter(|&b| held(b)) {
        for at in view.block(Block(b as u32)) {
            let op = view.op(at);
            let mut k = 0usize;
            each_value(view, at, |v| {
                let through = (op == Op::Load && k == 0)
                    || (op == Op::Store && k == 1)
                    || (matches!(op, Op::Memcpy | Op::Memmove | Op::Memcmp) && (k == 0 || k == 1))
                    || (op == Op::Memset && k == 0);
                if !through && view.op(v as usize) == Op::SlotAddr {
                    out[view.data[v as usize] as usize] = true;
                }
                k += 1;
            });
        }
    }
    // Forward may-reach over unescaped slots from every suspend's resume and cancel block:
    // bit `s` in `live_in[b]` means pre-suspend bytes of unescaped slot `s` may still be
    // unoverwritten at entry to block `b`.
    let words = slots.div_ceil(64);
    let ptr_bits = view.head.ptr_bits;
    let bit_get = |bits: &[u64], s: usize| bits.get(s / 64).is_some_and(|&w| w & (1u64 << (s % 64)) != 0);
    let bit_set = |bits: &mut [u64], s: usize| {
        if let Some(w) = bits.get_mut(s / 64) {
            *w |= 1u64 << (s % 64);
        }
    };
    let bit_clear = |bits: &mut [u64], s: usize| {
        if let Some(w) = bits.get_mut(s / 64) {
            *w &= !(1u64 << (s % 64));
        }
    };
    let union_into = |dst: &mut [u64], src: &[u64]| {
        let mut grew = false;
        for (d, &s) in dst.iter_mut().zip(src.iter()) {
            let next = *d | s;
            grew |= next != *d;
            *d = next;
        }
        grew
    };
    let mut unescaped = filled(words, 0u64, alloc);
    for (s, &escaped) in out.iter().enumerate() {
        if !escaped {
            bit_set(&mut unescaped, s);
        }
    }
    let mut live_in = filled(blocks * words, 0u64, alloc);
    let mut queued = filled(blocks, false, alloc);
    let mut work: blaze_own::Vec<Block, A> = blaze_own::Vec::new_in(*alloc);
    for s in body.suspends.iter() {
        for b in [Block(s.resume), Block(s.cancel)] {
            let bi = b.index();
            if !held(bi) {
                continue;
            }
            let Some(row) = live_in.get_mut(bi * words..(bi + 1) * words) else {
                continue;
            };
            if union_into(row, &unescaped)
                && let Some(q) = queued.get_mut(bi)
                && !*q
            {
                *q = true;
                work.push(b);
            }
        }
    }
    let mut cur = filled(words, 0u64, alloc);
    let slot_of = |v: crate::Value| -> Option<usize> {
        let ix = v.index();
        (view.op(ix) == Op::SlotAddr).then(|| view.data.get(ix).copied().and_then(|d| usize::try_from(d).ok())).flatten()
    };
    let slot_size = |s: usize| -> Option<u32> { view.slots.get(s).map(|slot| slot.size) };
    let iconst_u32 = |v: crate::Value| -> Option<u32> {
        let ix = v.index();
        (view.op(ix) == Op::Iconst)
            .then(|| view.data.get(ix).copied().map(u32::cast_signed).and_then(|n| u32::try_from(n).ok()))
            .flatten()
    };
    let mark_read = |out: &mut [bool], cur: &[u64], s: usize| {
        if bit_get(cur, s)
            && let Some(slot_out) = out.get_mut(s)
        {
            *slot_out = true;
        }
    };
    while let Some(b) = work.pop() {
        let bi = b.index();
        if let Some(q) = queued.get_mut(bi) {
            *q = false;
        }
        let Some(row) = live_in.get(bi * words..(bi + 1) * words) else {
            continue;
        };
        cur.copy_from_slice(row);
        for at in view.block(b) {
            match view.op(at) {
                Op::Load => {
                    let mut o = view.operands(at);
                    if let Some(s) = slot_of(o.value()) {
                        mark_read(&mut out, &cur, s);
                    }
                }
                Op::Store => {
                    let mut o = view.operands(at);
                    let val = o.value();
                    let ptr = o.value();
                    let off = o.imm().cast_signed();
                    if let Some(s) = slot_of(ptr)
                        && let Some(size) = slot_size(s)
                        && let Some(&raw_ty) = view.tys.get(val.index())
                    {
                        let bits = Ty(raw_ty).bits(ptr_bits);
                        if off == 0 && bits >= 8 && bits / 8 >= size {
                            bit_clear(&mut cur, s);
                        }
                    }
                }
                Op::Memcpy | Op::Memmove => {
                    let mut o = view.operands(at);
                    let dst = o.value();
                    let src = o.value();
                    let len = o.value();
                    if let Some(s) = slot_of(src) {
                        mark_read(&mut out, &cur, s);
                    }
                    if let Some(s) = slot_of(dst)
                        && let Some(size) = slot_size(s)
                        && let Some(n) = iconst_u32(len)
                        && n >= size
                    {
                        bit_clear(&mut cur, s);
                    }
                }
                Op::Memset => {
                    let mut o = view.operands(at);
                    let dst = o.value();
                    let _byte = o.value();
                    let len = o.value();
                    if let Some(s) = slot_of(dst)
                        && let Some(size) = slot_size(s)
                        && let Some(n) = iconst_u32(len)
                        && n >= size
                    {
                        bit_clear(&mut cur, s);
                    }
                }
                Op::Memcmp => {
                    let mut o = view.operands(at);
                    for p in [o.value(), o.value()] {
                        if let Some(s) = slot_of(p) {
                            mark_read(&mut out, &cur, s);
                        }
                    }
                }
                Op::SlotDead => {
                    if let Some(s) = view.data.get(at).copied().and_then(|d| usize::try_from(d).ok()) {
                        bit_clear(&mut cur, s);
                    }
                }
                _ => {}
            }
        }
        let term = view.terminator(b);
        if view.op(term) == Op::Suspend {
            continue;
        }
        successors(view, term, |to, _| {
            let ti = to.index();
            if !held(ti) {
                return;
            }
            let Some(row) = live_in.get_mut(ti * words..(ti + 1) * words) else {
                return;
            };
            if union_into(row, &cur)
                && let Some(q) = queued.get_mut(ti)
                && !*q
            {
                *q = true;
                work.push(to);
            }
        });
    }
    out
}

/// The kept slots in a laid state, and per slot its offset (`NONE`: not kept): every named slot
/// kept at its name's offset; every other slot not kept, unless the pass's rule keeps it
/// (`lir_kept`) and code after a suspension reaches it, which is refused by name. Refused
/// too: a name of no slot or of one slot twice, a name outside the region or misaligned for
/// its slot, two named slots sharing bytes.
pub(super) fn kept_by_name<A: Allocator + Copy>(
    body: &Body<'_, A>,
    lir_kept: &[bool],
    region: Region,
    names: &[SlotName],
    alloc: &A,
) -> Result<(Vec<bool, A>, Vec<u32, A>), CoroutineError> {
    let view = &body.view;
    let slots = view.slots.len();
    let mut kept = filled(slots, false, alloc);
    let mut at = filled(slots, NONE, alloc);
    for n in names {
        let i = n.slot.index();
        if i >= slots || at[i] != NONE {
            return Err(CoroutineError::SlotName { slot: n.slot.0 });
        }
        let rec = view.slots[i];
        let end = u64::from(n.offset) + u64::from(rec.size);
        let aligned = rec.align < 32 && n.offset % (1u32 << rec.align) == 0;
        if n.offset < region.start || end > u64::from(region.end) || !aligned {
            return Err(CoroutineError::NameOutside { slot: n.slot.0, offset: n.offset });
        }
        at[i] = n.offset;
        kept[i] = true;
    }
    // Two named slots sharing bytes: by offset, each must end before the next begins.
    let mut by_offset: Vec<(u32, u32), A> = Vec::with_capacity_in(names.len(), *alloc);
    by_offset.extend(names.iter().map(|n| (n.offset, n.slot.0)));
    by_offset.sort_unstable();
    for w in by_offset.windows(2) {
        let end = u64::from(w[0].0) + u64::from(view.slots[w[0].1 as usize].size);
        if end > u64::from(w[1].0) {
            return Err(CoroutineError::NamesOverlap { a: w[0].1, b: w[1].1 });
        }
    }
    let reached = reached_after(body, alloc);
    for i in 0..slots {
        if lir_kept[i] && at[i] == NONE && reached[i] {
            return Err(CoroutineError::UnnamedAcrossSuspend { slot: i as u32 });
        }
    }
    Ok((kept, at))
}

/// The offsets of the kept slots in slot order (what `overlap::offsets` gives without a region).
pub(super) fn placed<A: Allocator + Copy>(kept: &[bool], at: &[u32], alloc: &A) -> Vec<u32, A> {
    let mut out = Vec::new_in(*alloc);
    out.extend(kept.iter().zip(at.iter()).filter(|it| *it.0).map(|it| *it.1));
    out
}

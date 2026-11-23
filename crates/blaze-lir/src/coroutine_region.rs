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

/// Per slot, whether a block after some suspension (a resume or cancel target, and every block
/// reached from one, unwind edges included) takes its address: by a `slot_addr` there, or by
/// using a `slot_addr` value made before; or whose address escapes anywhere (below).
pub(super) fn reached_after<A: Allocator + Copy>(body: &Body<'_, A>, alloc: &A) -> Vec<bool, A> {
    let view = &body.view;
    let blocks = view.blocks();
    let held = |b: usize| body.in_poll[b] || body.in_drop[b];
    let mut seen = filled(blocks, false, alloc);
    let mut work: Vec<u32, A> = Vec::new_in(*alloc);
    for s in body.suspends.iter() {
        for b in [s.resume, s.cancel] {
            if !seen[b as usize] {
                seen[b as usize] = true;
                work.push(b);
            }
        }
    }
    while let Some(b) = work.pop() {
        if !held(b as usize) {
            continue;
        }
        successors(view, view.terminator(Block(b)), |to, _| {
            if !seen[to.index()] {
                seen[to.index()] = true;
                work.push(to.0);
            }
        });
    }
    let mut out = filled(view.slots.len(), false, alloc);
    for b in (0..blocks).filter(|&b| seen[b] && held(b)) {
        for at in view.block(Block(b as u32)) {
            if view.op(at) == Op::SlotAddr {
                out[view.data[at] as usize] = true;
            }
            each_value(view, at, |v| {
                if view.op(v as usize) == Op::SlotAddr {
                    out[view.data[v as usize] as usize] = true;
                }
            });
        }
    }
    // An address that leaves its slot's own loads and stores anywhere in the body (stored to
    // memory, passed to a call, made into a derived pointer) may be read after a suspension
    // through another place, which no `slot_addr` after it shows: such a slot counts as
    // reached. Only a `load` through it (its first operand) and a `store` through it (its
    // second; the first is the value stored) keep it unescaped.
    for b in (0..blocks).filter(|&b| held(b)) {
        for at in view.block(Block(b as u32)) {
            let op = view.op(at);
            let mut k = 0usize;
            each_value(view, at, |v| {
                let through = (op == Op::Load && k == 0) || (op == Op::Store && k == 1);
                if !through && view.op(v as usize) == Op::SlotAddr {
                    out[view.data[v as usize] as usize] = true;
                }
                k += 1;
            });
        }
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

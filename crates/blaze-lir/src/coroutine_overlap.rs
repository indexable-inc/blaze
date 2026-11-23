//! Overlap of kept slots (ASYNC.md D-A6; the design's D8; a child of [`super`], the coroutine
//! pass, so it reads the pass's own tables).
//!
//! Without overlap a state is the SUM of every slot it ever keeps: an `async fn` awaiting `k`
//! futures one after another, each awaiting `k` more, `d` levels deep, holds `k^d` leaves,
//! where the Reference's "Await expressions" (a future is "roughly equivalent to an enum with
//! one variant per await point") gives the largest of them. `#[tokio::main]` and
//! `#[tokio::test]` put the root future on the calling thread's stack, so the sum does not fit.
//!
//! **The rule.** Two kept slots may share bytes when no program point of the body has both
//! storage-live. Storage-live is the pass's own may-analysis (the module's first rule, in
//! [`addressed`]): made by `slot_live` or `slot_addr`, ended by `slot_dead`; an address taken
//! keeps a slot live to its `slot_dead`. It runs over every block `poll` or `drop` holds, across
//! suspends' resume and cancel edges and invokes' unwind edges, so a slot live on any path into
//! a point counts there. Sound because a slot's bytes are reached only through an address its
//! `slot_addr` made: between its `slot_dead` and its next `slot_live` or `slot_addr` nothing
//! reads them, and another slot's bytes written meanwhile are bytes this one no longer has.
//!
//! **The conflicts** ([`conflicts`]) are a bit matrix over the kept slots, in slot order: row
//! `i` holds `j` when some point has both live. Walking each held block forward from the slots
//! live where it starts: every pair live at its start conflicts, and a slot that becomes live
//! conflicts with every slot live there.
//!
//! **The placement** ([`offsets`]) is greedy, by falling size then slot index (a total key,
//! so the same bytes on any machine): each slot at the lowest offset of its alignment, from the
//! first byte past the upvars, that overlaps no conflicting slot already placed. Values stay
//! one field each, past the slots (a value is stored once where it is made and loaded at each
//! use; its own life is not tracked by markers).

use alloc::vec::Vec;
use alloc::alloc::Allocator;
use core::cmp::Reverse;

use super::{Body, CoroutineError, Plant, align_up, filled, successors};
use crate::{Block, Op, func::NONE};

/// Per held block, the slots whose address may have been taken (or whose storage begun) and
/// whose storage not ended, where the block starts and where it ends: one bit per slot of the
/// body, `words` words per block.
pub(super) struct Addressed<A: Allocator> {
    pub(super) words: usize,
    pub(super) at_start: Vec<u64, A>,
    pub(super) at_end: Vec<u64, A>,
}

/// The pass's may-analysis of slot storage (the module's first rule): the one place it is
/// computed, read by `kept_slots` (which slots a suspend sees) and by [`conflicts`] (which
/// pairs share a point).
pub(super) fn addressed<A: Allocator + Copy>(body: &Body<'_, A>, alloc: &A) -> Addressed<A> {
    let view = &body.view;
    let (blocks, slots) = (view.blocks(), view.slots.len());
    let words = slots.div_ceil(64);
    let held = |b: usize| body.in_poll[b] || body.in_drop[b];
    // Per block: the slots whose last marker in it is an address or a `slot_live` (`made`),
    // or a `slot_dead` (`ended`); the slots that may be addressed where it starts and ends.
    let mut made = filled(blocks * words, 0u64, alloc);
    let mut ended = filled(blocks * words, 0u64, alloc);
    let mut at_start = filled(blocks * words, 0u64, alloc);
    let mut at_end = filled(blocks * words, 0u64, alloc);
    for b in (0..blocks).filter(|&b| held(b)) {
        for at in view.block(Block(b as u32)) {
            let op = view.op(at);
            if !matches!(op, Op::SlotAddr | Op::SlotLive | Op::SlotDead) {
                continue;
            }
            let slot = view.data[at] as usize;
            let (word, bit) = (b * words + slot / 64, 1u64 << (slot % 64));
            if op == Op::SlotDead {
                made[word] &= !bit;
                ended[word] |= bit;
            } else {
                made[word] |= bit;
                ended[word] &= !bit;
            }
        }
    }
    // The least fixed point, by a work list: a block is done again when its start grew.
    let mut queued = filled(blocks, false, alloc);
    let mut work = Vec::with_capacity_in(blocks, *alloc);
    for b in (0..blocks).rev().filter(|&b| held(b)) {
        queued[b] = true;
        work.push(b as u32);
    }
    while let Some(b) = work.pop() {
        let b = b as usize;
        queued[b] = false;
        for w in 0..words {
            at_end[b * words + w] = made[b * words + w] | (at_start[b * words + w] & !ended[b * words + w]);
        }
        successors(view, view.terminator(Block(b as u32)), |to, _| {
            let to = to.index();
            let mut grew = false;
            for w in 0..words {
                let out = at_end[b * words + w];
                let start = &mut at_start[to * words + w];
                grew |= out & !*start != 0;
                *start |= out;
            }
            if grew && !queued[to] {
                queued[to] = true;
                work.push(to as u32);
            }
        });
    }
    Addressed { words, at_start, at_end }
}

/// Calls `f` with the index of each bit set in `bits`, rising.
fn each_bit(bits: &[u64], mut f: impl FnMut(usize)) {
    for (w, &word) in bits.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            f(w * 64 + rest.trailing_zeros() as usize);
            rest &= rest - 1;
        }
    }
}

/// The conflict matrix of the kept slots (those `kept` marks, numbered by rising slot): row
/// `i` (`kept_count.div_ceil(64)` words) holds bit `j` when some instruction of a held block
/// has both storage-live. A row may hold its own bit; [`offsets`] ignores it.
pub(super) fn conflicts<A: Allocator + Copy>(
    body: &Body<'_, A>,
    live: &Addressed<A>,
    kept: &[bool],
    plant: Plant,
    alloc: &A,
) -> Vec<u64, A> {
    let view = &body.view;
    let mut ordinal = filled(kept.len(), NONE, alloc);
    let mut n = 0usize;
    for (slot, &keep) in kept.iter().enumerate() {
        if keep {
            ordinal[slot] = n as u32;
            n += 1;
        }
    }
    let rw = n.div_ceil(64);
    let mut matrix = filled(n * rw, 0u64, alloc);
    // Planted (test builds only): every pair conflicts, so no two kept slots share bytes.
    if cfg!(test) && plant == Plant::NoOverlap {
        for i in 0..n {
            for j in 0..n {
                matrix[i * rw + j / 64] |= 1u64 << (j % 64);
            }
        }
        return matrix;
    }
    // Planted (test builds only): no pair conflicts, so every kept slot starts at one offset.
    if cfg!(test) && plant == Plant::OverlapConflicting {
        return matrix;
    }
    if n == 0 {
        return matrix;
    }
    let words = live.words;
    let mut cur = filled(rw, 0u64, alloc);
    for b in (0..view.blocks()).filter(|&b| body.in_poll[b] || body.in_drop[b]) {
        cur.fill(0);
        each_bit(&live.at_start[b * words..(b + 1) * words], |slot| {
            let i = ordinal[slot];
            if i != NONE {
                cur[i as usize / 64] |= 1u64 << (i % 64);
            }
        });
        // Every pair live where the block starts conflicts (a join of two paths, each
        // making one of them live, is such a point).
        each_bit(&cur, |i| {
            for w in 0..rw {
                matrix[i * rw + w] |= cur[w];
            }
        });
        for at in view.block(Block(b as u32)) {
            let op = view.op(at);
            if !matches!(op, Op::SlotAddr | Op::SlotLive | Op::SlotDead) {
                continue;
            }
            let i = ordinal[view.data[at] as usize];
            if i == NONE {
                continue;
            }
            let (word, bit) = (i as usize / 64, 1u64 << (i % 64));
            if op == Op::SlotDead {
                cur[word] &= !bit;
                continue;
            }
            if cur[word] & bit != 0 {
                continue;
            }
            cur[word] |= bit;
            // Slot `i` begins here: it conflicts with every slot live here, both ways.
            let row = i as usize * rw;
            for w in 0..rw {
                matrix[row + w] |= cur[w];
            }
            each_bit(&cur, |j| matrix[j * rw + word] |= bit);
        }
    }
    matrix
}

/// The offset of each kept slot (`sizes[i]` its size and the log2 of its alignment, in the
/// numbering of [`conflicts`]), every one at or past `start`: greedy by falling size then
/// index, each at the lowest aligned offset overlapping no conflicting slot placed before it.
/// Refused: an end past what a 32-bit offset names.
pub(super) fn offsets<A: Allocator + Copy>(
    sizes: &[(u32, u8)],
    conflicts: &[u64],
    start: u32,
    alloc: &A,
) -> Result<Vec<u32, A>, CoroutineError> {
    let n = sizes.len();
    let rw = n.div_ceil(64);
    let mut order = Vec::with_capacity_in(n, *alloc);
    order.extend(0..n);
    order.sort_unstable_by_key(|&i| (Reverse(sizes[i].0), i));
    let mut offset = filled(n, 0u32, alloc);
    let mut placed = filled(n, false, alloc);
    // The placed slots `i` conflicts with, as byte ranges, by rising start.
    let mut taken: Vec<(u64, u64), A> = Vec::with_capacity_in(n, *alloc);
    for &i in order.iter() {
        let (size, align) = sizes[i];
        taken.clear();
        for j in 0..n {
            if j != i && placed[j] && conflicts[i * rw + j / 64] & (1u64 << (j % 64)) != 0 {
                let at = u64::from(offset[j]);
                taken.push((at, at + u64::from(sizes[j].0)));
            }
        }
        taken.sort_unstable();
        let mut at = align_up(u64::from(start), align);
        for &(first, end) in taken.iter() {
            if end <= at {
                continue;
            }
            if first >= at + u64::from(size) {
                break;
            }
            at = align_up(end, align);
        }
        let end = at + u64::from(size);
        if end > u64::from(u32::MAX) {
            return Err(CoroutineError::TooLarge);
        }
        offset[i] = at as u32;
        placed[i] = true;
    }
    Ok(offset)
}

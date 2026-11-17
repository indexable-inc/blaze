//! A coroutine's state layout as a pure function of TYPES' sizes (ASYNC.md P8, option (b) as main
//! decided it, 2026-11-16): the upvars first, then one field per saved local of the block's
//! flow (blaze-flow-lower state_recipe.rs), then the tag. Made BEFORE any LIR, so the LIR split
//! places what it keeps INSIDE the region this gives ([`StateRegion`]) and never decides a size.
//!
//! Order: the upvars at offset 0 (the builder's own prefix, as today); the saved fields by
//! falling alignment, then by their index in the recipe (deterministic: rule 25); the tag last,
//! in the narrowest of `i8`, `i16`, `i32` holding `Unresumed` 0, `Returned` 1, `Panicked` 2 and
//! point `k` as `3 + k` (blaze-lir coroutine.rs's numbering). The Reference leaves the layout of
//! coroutines unspecified; only what is held is its ("Async blocks": "one variant per await
//! point, where each variant stores the data needed to resume").
//!
//! FIRST CUT (stated, owed): no two saved fields share bytes. P6's overlap (blaze-lir
//! coroutine_overlap.rs) packs the LIR's kept slots tighter INSIDE the region; the region's
//! size shrinks only with the flow-level follow-up (state_recipe.rs's module doc).

use blaze_lir::Ty;
use blaze_own::{Alloc, Vec};

use crate::{BuildError, StateShape};

/// The bytes of a state the LIR split may place its kept slots and values in:
/// `start..end`, past the upvars, before the tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateRegion {
    pub start: u32,
    pub end: u32,
    /// The log2 of the alignment `start` has.
    pub align: u8,
}

/// A state laid out from types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LaidState {
    pub shape: StateShape,
    pub region: StateRegion,
}

/// The most alignment (log2) a state field may ask (blaze-lir's `CoroutineError::TooLarge` bound).
const MOST_ALIGN: u8 = 31;

fn align_up(at: u64, align: u8) -> u64 {
    let a = 1u64 << align;
    (at + a - 1) & !(a - 1)
}

/// The state of a coroutine whose upvars' tuple is `env` (size, log2 align), whose saved
/// locals' ground types have the layouts `saved` (size, log2 align; the recipe's order), and
/// whose body has `points` suspensions. Refused by name: an alignment past 2^31, a state past
/// `u32::MAX` bytes, more points than an `i32` tag numbers.
pub fn lay_state(env: (u64, u8), saved: &[(u64, u8)], points: u32, scratch: Alloc<'_>) -> Result<LaidState, BuildError> {
    let too_big = BuildError::UnsupportedSig { what: "a coroutine state past 4 GiB" };
    let too_aligned = BuildError::UnsupportedSig { what: "a coroutine state field aligned past 2^31" };
    if env.1 > MOST_ALIGN || saved.iter().any(|f| f.1 > MOST_ALIGN) {
        return Err(too_aligned);
    }
    let tags = u64::from(points) + 3;
    let tag_ty = if tags <= 1 << 8 {
        Ty::I8
    } else if tags <= 1 << 16 {
        Ty::I16
    } else if tags <= 1 << 31 {
        Ty::I32
    } else {
        return Err(BuildError::UnsupportedSig { what: "a coroutine with more suspensions than an i32 tag numbers" });
    };
    let tag_align: u8 = match tag_ty {
        Ty::I8 => 0,
        Ty::I16 => 1,
        _ => 2,
    };
    // The saved fields by falling alignment, then recipe index.
    let mut order: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(saved.len(), scratch);
    order.extend(0..saved.len() as u32);
    // Keys are unique (the index is in them): an unstable sort is deterministic.
    order.sort_unstable_by_key(|&i| (core::cmp::Reverse(saved[i as usize].1), i));
    let region_align = saved.iter().map(|f| f.1).max().unwrap_or(0);
    let start = align_up(env.0, region_align);
    let mut at = start;
    for &i in order.iter() {
        let (size, align) = saved[i as usize];
        at = align_up(at, align).checked_add(size).ok_or(BuildError::UnsupportedSig { what: "a coroutine state past 4 GiB" })?;
    }
    let tag_offset = align_up(at, tag_align);
    let align = env.1.max(region_align).max(tag_align);
    let size = align_up(tag_offset + (1u64 << tag_align), align);
    if size > u64::from(u32::MAX) {
        return Err(too_big);
    }
    // Every value below fits: `size` does.
    let shape = StateShape { size: size as u32, align, tag_offset: tag_offset as u32, tag_ty };
    Ok(LaidState { shape, region: StateRegion { start: start as u32, end: tag_offset as u32, align: region_align } })
}

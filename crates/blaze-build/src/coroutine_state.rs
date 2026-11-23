//! A coroutine's state layout as a pure function of TYPES' sizes (ASYNC.md P8, option (b) as main
//! decided it, 2026-11-16): the upvars first, then one field per saved local of the block's
//! flow (blaze-flow-lower state_recipe.rs), then one drop flag byte per saved local whose type
//! needs drop, then the tag. Made BEFORE any LIR.
//!
//! Each saved local has its OWN offset ([`Laid::field`], [`Laid::flag`]), keyed by its identity
//! in the flow (blaze-flow-lower identity.rs) through [`StateName`]: the builder looks a slot it
//! keeps across a suspension up by that key and the LIR split places the slot at that offset
//! (blaze-lir coroutine_region.rs). The flow layer is the one owner of which locals cross a
//! suspension (main's ruling, async-P8-4); nothing here or in the split decides it again.
//!
//! Order: the upvars at offset 0 (the builder's own prefix, as today); the saved fields by
//! falling alignment, then by their index in the recipe (deterministic: rule 25); the flags in
//! recipe order; the tag last, in the narrowest of `i8`, `i16`, `i32` holding `Unresumed` 0,
//! `Returned` 1, `Panicked` 2 and point `k` as `3 + k` (blaze-lir coroutine.rs's numbering). The
//! Reference leaves the layout of coroutines unspecified; only what is held is its ("Async
//! blocks": "one variant per await point, where each variant stores the data needed to
//! resume").
//!
//! FIRST CUT (stated, owed): no two saved fields share bytes, even when no suspension holds
//! both (the flow-level overlap follow-up, state_recipe.rs's module doc).

use core::alloc::Allocator;

use blaze_flow_lower::identity::Identity;
use blaze_lir::Ty;
use blaze_own::{Alloc, Vec};

use crate::{BuildError, StateShape};

/// The bytes of a state past the upvars and before the tag: the saved fields and the flags.
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
    /// The tag's valid range (`Unresumed` 0 to the last point's `2 + points`): what
    /// `LayoutFacts::coroutine_niche` answers, from the types too.
    pub niche: crate::StateNiche,
}

/// [`lay_state`]'s answer: the state, and per saved local (the recipe's order) its field's
/// offset and its drop flag's offset (`None`: its type needs no drop).
pub struct Laid<A: Allocator> {
    pub state: LaidState,
    pub field: Vec<u32, A>,
    pub flag: Vec<Option<u32>, A>,
}

/// One saved local as the builder looks it up: its identity in the flow, its field's offset
/// and size, its drop flag's offset. A table of these is sorted by `identity`, each once
/// (state_recipe.rs refuses a saved identity twice).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateName {
    pub identity: Identity,
    pub offset: u32,
    pub size: u32,
    pub flag: Option<u32>,
}

/// What `build_coroutine` builds a coroutine into: the state laid from types and the table of
/// its saved locals by identity ([`StateNames::new`] checked it sorted and unique once).
#[derive(Clone, Copy)]
pub struct StatePlan<'a> {
    pub laid: LaidState,
    pub names: StateNames<'a>,
    /// A planted bug for this crate's tests; it acts only in this crate's test builds.
    pub plant: StatePlant,
}

/// The `what` of `BuildError::Unsupported` for a slot the walk keeps across a suspension that
/// the flow's table does not name, at the expression whose task made the slot (walk/state_names.rs).
pub const UNNAMED_ACROSS: &str = "a slot this expression made is read after a suspension, and the flow saves no local it names (only flow locals cross a suspension: ASYNC.md P8, async-P8-4); the flow body must name the temporary, or the walk end the slot before the `.await`";

/// A planted bug for the tests' controls (blaze-build walk/state_names.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatePlant {
    None,
    /// Each `.await` makes a slot of the walk's own before its loop and reads it after the
    /// resume: a builder-made slot across a suspension, which the split must refuse.
    BuilderSlot,
}

/// A sorted, unique table of [`StateName`]s: the proof that a lookup has one answer.
#[derive(Clone, Copy)]
pub struct StateNames<'a> {
    rows: &'a [StateName],
}

impl<'a> StateNames<'a> {
    /// `rows`, checked sorted by identity with no identity twice and every field and flag
    /// inside `region`. Refused by name otherwise (a caller's bug: the rows come from one
    /// recipe and one [`lay_state`]).
    pub fn new(rows: &'a [StateName], region: StateRegion) -> Result<StateNames<'a>, BuildError> {
        if rows.windows(2).any(|w| w[0].identity >= w[1].identity) {
            return Err(BuildError::Malformed { what: "a coroutine state's names not sorted by identity, or one identity twice" });
        }
        let inside = |at: u32, len: u32| at >= region.start && u64::from(at) + u64::from(len) <= u64::from(region.end);
        if rows.iter().any(|r| !inside(r.offset, r.size) || r.flag.is_some_and(|f| !inside(f, 1))) {
            return Err(BuildError::Malformed { what: "a coroutine state's saved field or flag outside its region" });
        }
        Ok(StateNames { rows })
    }

    /// The saved local of identity `id`, if the flow saves one.
    pub fn get(self, id: Identity) -> Option<StateName> {
        self.rows.binary_search_by_key(&id, |r| r.identity).ok().map(|i| self.rows[i])
    }
}

/// The most alignment (log2) a state field may ask (blaze-lir's `CoroutineError::TooLarge` bound).
const MOST_ALIGN: u8 = 31;

fn align_up(at: u64, align: u8) -> u64 {
    let a = 1u64 << align;
    (at + a - 1) & !(a - 1)
}

/// The state of a coroutine whose upvars' tuple is `env` (size, log2 align), whose saved
/// locals' ground types have the layouts `saved` (size, log2 align; the recipe's order), of
/// which those `needs_flag` marks need drop (one `u8` drop flag each, after the saved fields:
/// main's ruling (ii), blaze-flow-lower state_recipe.rs), and whose body has `points`
/// suspensions; the offsets in `alloc`. Refused by name: `needs_flag` not one row per saved
/// local, an alignment past 2^31, a state past `u32::MAX` bytes, more points than an `i32` tag
/// numbers.
pub fn lay_state<A: Allocator + Copy>(
    env: (u64, u8),
    saved: &[(u64, u8)],
    needs_flag: &[bool],
    points: u32,
    scratch: Alloc<'_>,
    alloc: A,
) -> Result<Laid<A>, BuildError> {
    // Built at each use: `BuildError` is not `Clone`.
    let too_big = || BuildError::UnsupportedSig { what: "a coroutine state past 4 GiB" };
    let too_aligned = BuildError::UnsupportedSig { what: "a coroutine state field aligned past 2^31" };
    if needs_flag.len() != saved.len() {
        return Err(BuildError::Malformed { what: "a coroutine state's drop-flag rows not one per saved local" });
    }
    if env.1 > MOST_ALIGN || saved.iter().any(|f| f.1 > MOST_ALIGN) {
        return Err(too_aligned);
    }
    let tags = u64::from(points) + 3;
    // The same widths as blaze-lir coroutine.rs `plan`.
    let (tag_ty, tag_align) = if tags <= 1 << 8 {
        (Ty::I8, 0u8)
    } else if tags <= 1 << 16 {
        (Ty::I16, 1)
    } else if tags <= 1 << 31 {
        (Ty::I32, 2)
    } else {
        return Err(BuildError::UnsupportedSig { what: "a coroutine with more suspensions than an i32 tag numbers" });
    };
    // The saved fields by falling alignment, then recipe index.
    let mut order: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(saved.len(), scratch);
    order.extend(0..saved.len() as u32);
    // Keys are unique (the index is in them): an unstable sort is deterministic.
    order.sort_unstable_by_key(|&i| (core::cmp::Reverse(saved[i as usize].1), i));
    let region_align = saved.iter().map(|f| f.1).max().unwrap_or(0);
    let start = align_up(env.0, region_align);
    let mut at = start;
    let mut field: Vec<u32, A> = Vec::with_capacity_in(saved.len(), alloc);
    field.resize(saved.len(), 0);
    for &i in order.iter() {
        let (size, align) = saved[i as usize];
        let here = align_up(at, align);
        field[i as usize] = u32::try_from(here).map_err(|_| too_big())?;
        at = here.checked_add(size).ok_or_else(too_big)?;
    }
    // The drop flags, a byte each, in recipe order.
    let mut flag: Vec<Option<u32>, A> = Vec::with_capacity_in(saved.len(), alloc);
    for &needs in needs_flag {
        if needs {
            flag.push(Some(u32::try_from(at).map_err(|_| too_big())?));
            at += 1;
        } else {
            flag.push(None);
        }
    }
    let tag_offset = align_up(at, tag_align);
    let align = env.1.max(region_align).max(tag_align);
    let size = align_up(tag_offset + (1u64 << tag_align), align);
    if size > u64::from(u32::MAX) {
        return Err(too_big());
    }
    // Every value below fits: `size` does; `points + 2` does, `tags` fit an i32 tag.
    let shape = StateShape { size: size as u32, align, tag_offset: tag_offset as u32, tag_ty };
    let niche = crate::StateNiche { tag_offset: tag_offset as u32, tag_ty, valid_end: (tags - 1) as u32 };
    let region = StateRegion { start: start as u32, end: tag_offset as u32, align: region_align };
    Ok(Laid { state: LaidState { shape, region, niche }, field, flag })
}

//! The tag's niche of a coroutine's state (ASYNC.md D-A7, P7).
//!
//! The pass (`blaze_lir::coroutine`, its module doc) stores in the state's tag only `Unresumed`
//! 0, `Returned` 1, `Panicked` 2 and point `k` as `3 + k`, so a state of `points` points holds
//! a tag in `0..=2 + points` at every moment a value of the future exists (a state never polled
//! holds 0 there). The tag's other values are a niche at `tag_offset`, as the module doc of
//! layout.rs says of any scalar "whose valid values are a proper range": an enclosing enum
//! (`Option<impl Future>`) encodes its other variants in them. The Reference does not specify
//! the layout of a future ("Async blocks": an async block "evaluates to a future"; no layout is
//! promised), so a smaller `Option<impl Future>` changes no program's behaviour.
//!
//! [`niche_of`] reads the valid range off the layout the pass returned; [`row_niche`] is that
//! range as a layout row's [`Niche`]; [`state_row_niche`] is what layout.rs's `PNode::Future`
//! arm calls: it checks an answer against the state's shape and refuses by name what it does
//! not hold.

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::Ty;
use blaze_lir::coroutine::{FIRST_POINT, StateLayout};

use crate::StateShape;
use crate::layout::{Niche, Prim, Scalar, WrappingRange, mask};

/// The tag of a coroutine instance's state and the values it may hold: `0..=valid_end`, where
/// `valid_end = 2 + points` (`blaze_lir::coroutine::FIRST_POINT - 1 + points`). Kept beside
/// the instance's [`StateShape`] by whoever ran the pass, and answered by
/// `LayoutFacts::coroutine_niche`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateNiche {
    pub tag_offset: u32,
    /// `i8`, `i16` or `i32`, the pass's choice.
    pub tag_ty: Ty,
    /// The highest tag the state may hold.
    pub valid_end: u32,
}

/// The tag's valid range of the layout the pass returned.
pub fn niche_of<A: Allocator>(layout: &StateLayout<A>) -> StateNiche {
    // The pass refuses a point past `u32::MAX - FIRST_POINT` (coroutine.rs, `plan`), so
    // `FIRST_POINT - 1 + points` fits a `u32`; saturating states that bound, not a fallback.
    let valid_end = (FIRST_POINT - 1).saturating_add(layout.points);
    StateNiche { tag_offset: layout.tag_offset, tag_ty: layout.tag_ty, valid_end }
}

/// The tag as a primitive: an unsigned integer of the tag's width (the pass compares and
/// switches on it zero-extended). Anything but `i8`, `i16` or `i32` is refused by name.
pub fn tag_prim(tag_ty: Ty) -> Result<Prim, NicheRefused> {
    let bytes = if tag_ty == Ty::I8 {
        1
    } else if tag_ty == Ty::I16 {
        2
    } else if tag_ty == Ty::I32 {
        4
    } else {
        return Err(NicheRefused::TagType);
    };
    Ok(Prim::Int { bytes, signed: false })
}

/// The niche of a state whose tag is `n`, in the form layout.rs documents (its module doc,
/// "Niches", and [`Niche`]): the scalar at `tag_offset` whose valid values are
/// `0..=valid_end`; the values after `valid_end` are what an enclosing enum may take.
///
/// `n.tag_ty` is one [`tag_prim`] accepts: [`niche_of`] only makes such, and
/// [`state_row_niche`] checks before it calls this. Another is a broken invariant of the
/// caller, and it fails this one job naming the type.
pub fn row_niche(n: StateNiche) -> Niche {
    let prim = match tag_prim(n.tag_ty) {
        Ok(prim) => prim,
        Err(refused) => panic!("{refused}: {:?}", n.tag_ty),
    };
    Niche { offset: u64::from(n.tag_offset), scalar: Scalar { prim, valid: WrappingRange { start: 0, end: u128::from(n.valid_end) } } }
}

/// The layout row's niche of a state of shape `state` whose tag the world answered as `n`:
/// `None` when the tag has no invalid value (every value of its width is a tag in use).
/// Refused by name: an answer about another tag than the shape's, a tag type the pass does not
/// make, a valid range past the tag's width, or a tag outside the state's bytes.
pub fn state_row_niche(n: StateNiche, state: StateShape) -> Result<Option<Niche>, NicheRefused> {
    if n.tag_offset != state.tag_offset || n.tag_ty != state.tag_ty {
        return Err(NicheRefused::OtherTag);
    }
    let prim = tag_prim(n.tag_ty)?;
    if u64::from(n.tag_offset) + prim.size() > u64::from(state.size) {
        return Err(NicheRefused::TagOutside);
    }
    if u128::from(n.valid_end) > mask(prim.bits()) {
        return Err(NicheRefused::RangePastWidth);
    }
    let niche = row_niche(n);
    Ok((niche.available() > 0).then_some(niche))
}

/// Why a state's niche was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NicheRefused {
    /// The niche's tag is not the shape's (offset or type): the world answered another
    /// instance's niche.
    OtherTag,
    /// The tag is not `i8`, `i16` or `i32`.
    TagType,
    /// The tag does not lie inside the state's bytes.
    TagOutside,
    /// `valid_end` does not fit the tag's width.
    RangePastWidth,
}

impl NicheRefused {
    /// What was refused, for `layout::Refusal::Malformed`.
    pub const fn what(self) -> &'static str {
        match self {
            NicheRefused::OtherTag => "a coroutine's tag niche about another tag than its state's (ASYNC.md D-A7)",
            NicheRefused::TagType => "a coroutine's tag that is not i8, i16 or i32 (ASYNC.md D-A7)",
            NicheRefused::TagOutside => "a coroutine's tag outside its state's bytes (ASYNC.md D-A7)",
            NicheRefused::RangePastWidth => "a coroutine's tag range past its tag's width (ASYNC.md D-A7)",
        }
    }
}

impl fmt::Display for NicheRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

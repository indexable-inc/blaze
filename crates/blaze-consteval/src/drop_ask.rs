//! The `needs_drop` intrinsic's answer at compile time (the pinned core, library/core/src/mem/
//! mod.rs: `pub const fn needs_drop<T: ?Sized>() -> bool { const { intrinsics::needs_drop::<T>()
//! } }`): the one rule, [`blaze_build::drop_facts::needs_drop`], asked over this world's facts
//! ([`ConstWorld::drop_impl`], [`ConstWorld::manually_drop`]); each way it cannot answer is a
//! [`Refusal`] naming what stopped it, never a guessed `false`.

use blaze_build::BuildError;
use blaze_build::drop_facts::{self, NeedsDropError};
use blaze_own::Alloc;
use blaze_typeck::{ItemKey, PNode, PTy, PortableTys};

use crate::{ConstWorld, LayoutFacts, Refusal};

/// Why the `Drop` impl question of one ADT type got no answer.
enum Ask {
    /// The world answers no `Drop` impl, or does not say which ADT is `ManuallyDrop`
    /// ([`ConstWorld::drop_impl`]'s and [`ConstWorld::manually_drop`]'s defaults): the ADT met.
    NotAnswered(ItemKey),
    /// The world answered with an error: a projection to normalize, or another.
    Build(BuildError),
}

/// Whether ground type `ty` (of `ground`) needs drop, as `core::intrinsics::needs_drop::<ty>()`.
/// A world that answers neither fact still answers every type that reaches no ADT (`u8`, a
/// tuple of scalars, a reference); the first ADT met is then refused naming it.
pub(crate) fn needs_drop<W: ConstWorld>(world: &W, ty: PTy, ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<bool, Refusal<'static>> {
    let manually_drop = world.manually_drop();
    let mut ask = |t: PTy, g: &mut PortableTys<'_>| -> Result<Option<ItemKey>, Ask> {
        let adt = match g.view().node(t) {
            Ok(PNode::Adt(k, _)) => k,
            _ => return Err(Ask::Build(BuildError::Malformed { what: "a `Drop` impl asked of a type that is not an ADT" })),
        };
        // Without the `ManuallyDrop` answer an ADT's fields cannot be judged (its value's drop
        // is inhibited): refused here, before its fields are read.
        if manually_drop.is_none() {
            return Err(Ask::NotAnswered(adt));
        }
        match world.drop_impl(t, g) {
            None => Err(Ask::NotAnswered(adt)),
            Some(Err(e)) => Err(Ask::Build(e)),
            Some(Ok(found)) => Ok(found),
        }
    };
    let facts: &dyn LayoutFacts = world;
    drop_facts::needs_drop(ty, ground, facts, manually_drop.flatten(), &mut ask, alloc).map_err(refusal)
}

/// [`NeedsDropError`] as the evaluator's refusal: a projection as the evaluator refuses one
/// elsewhere (pending: the front end makes the impls and asks again), a missing definition or
/// answer as [`Refusal::Missing`], the rest by name as [`Refusal::NeedsDrop`].
fn refusal(e: NeedsDropError<Ask>) -> Refusal<'static> {
    let build = |e: BuildError, what: &'static str| match e {
        BuildError::Projection { trait_, name } => Refusal::Projection { trait_, name },
        _ => Refusal::NeedsDrop { what },
    };
    match e {
        NeedsDropError::Ask(Ask::NotAnswered(adt)) => Refusal::Missing { what: "`Drop` impl or `ManuallyDrop` answer (`needs_drop`)", item: adt },
        NeedsDropError::Ask(Ask::Build(e)) => build(e, "the `Drop` impl of an ADT part could not be selected"),
        NeedsDropError::Ground(e) => build(e, "a part's type (a field or a capture) has no ground form"),
        NeedsDropError::NotGround { what, .. } => Refusal::NeedsDrop { what },
        NeedsDropError::ArrayLength { .. } => Refusal::NeedsDrop { what: "an array length that is not a ground `usize` value" },
        NeedsDropError::NoAdt { adt } => Refusal::Missing { what: "ADT definition", item: adt },
        NeedsDropError::NoClosure { owner, .. } => Refusal::Missing { what: "closure facts", item: owner },
        NeedsDropError::Malformed { what } => Refusal::Malformed { what },
        NeedsDropError::TooManyTypes { .. } => Refusal::NeedsDrop { what: "a type reaching more distinct types than the walk's cap" },
    }
}

//! An ADT's `Drop` impl, found by the type's head alone ([`adt_drop_impl`]).
//!
//! Only a struct, enum or union can implement `Drop` (E0120: "the Drop trait may only be
//! implemented for structs, enums, and unions"; the Reference, "Destructors": "If T implements
//! Drop ... then ... the destructor of T"; a closure, tuple, array, pointer or trait object drops
//! only its parts), and a `Drop` impl applies to EVERY instance of its ADT: it may not be
//! specialized (E0366/E0367: the impl must have the ADT's own generic parameters and bounds).
//! So which impl drops a value is a question of the ADT's item alone: the arguments are never
//! imported, and nothing is selected by the solver (a closure among them would need its owner's
//! checked record, which the run path's world need not hold).

use crate::ids::ItemKey;
use crate::instance::ResolveError;
use crate::portable::PNode;
use crate::world::{ImplPolarity, Question, Reads, Shape, World};
use blaze_own::Alloc;

/// The one positive impl of the `drop` lang trait `drop_trait` whose self type is the ADT
/// `adt` (any arguments), from the impls in the bucket `Shape::Adt(adt)`; `Ok(None)` when the
/// ADT has none. Two such impls are a malformed library (coherence refuses the second):
/// [`ResolveError::Overlap`] naming both. An impl the world lists but cannot describe is
/// [`ResolveError::Missing`] naming its header.
pub fn adt_drop_impl<W: World>(world: &W, drop_trait: ItemKey, adt: ItemKey, alloc: Alloc<'_>) -> Result<Option<ItemKey>, ResolveError> {
    let mut reads = Reads::new_in(world, alloc);
    // An unmade bucket is a hole whose stand-in lists no impl: "no `Drop`" is no answer then.
    let impls = reads.impls_of(drop_trait, Shape::Adt(adt))?;
    if let Some(q) = reads.first_hole() {
        return Err(ResolveError::Missing(q));
    }
    let mut found: Option<ItemKey> = None;
    for &imp in impls {
        let Some(h) = reads.impl_header(imp)? else { return Err(ResolveError::Missing(Question::ImplHeader(imp))) };
        if h.trait_ != Some(drop_trait) || h.polarity != ImplPolarity::Positive {
            continue;
        }
        let PNode::Adt(k, _) = h.types.node(h.self_ty)? else { continue };
        if k != adt {
            continue;
        }
        if let Some(first) = found {
            return Err(ResolveError::Overlap { trait_: drop_trait, first, second: imp });
        }
        found = Some(imp);
    }
    Ok(found)
}

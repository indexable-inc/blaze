//! The linear simplify stage, run whole (docs/v2/plan/design/backend-next.md, D100 point 1):
//! what the Fast tier takes is a body after the trivial inliner ([`inline_trivial`]), scalar
//! replacement ([`scalar_replace`]), copy and constant propagation ([`propagate`]) and dead
//! code and trivial branch removal ([`dead`]), in that order, each once.

use alloc::vec::Vec;
use core::alloc::Allocator;

use super::{SimplifyError, dead, propagate, scalar_replace};
use crate::{
    Func, View,
    body::{Bodies, BodyId},
    inline::{Bound, InlineError, inline_trivial},
};

/// The body `view` (whose identity is `root`; its callees from `bodies`) through the whole
/// stage, in `alloc`, verified; with the bodies the inliner spliced into it, sorted, unique:
/// exactly what the fast record's read list gains.
pub fn simplify<A: Allocator + Copy>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    alloc: A,
) -> Result<(Func<A>, Vec<BodyId, A>), SimplifyError> {
    let inlined = inline_trivial(view, root, bodies, bound, alloc).map_err(from_inline)?;
    let replaced = scalar_replace(inlined.func.view(), alloc)?;
    let propagated = propagate(replaced.view(), alloc)?;
    let func = dead(propagated.view(), alloc)?;
    Ok((func, inlined.read))
}

/// The inliner's refusal as the stage's: the same three kinds, unchanged.
fn from_inline(error: InlineError) -> SimplifyError {
    match error {
        InlineError::Build(error) => SimplifyError::Build(error),
        InlineError::Verify(error) => SimplifyError::Verify(error),
        InlineError::Refused { inst, what } => SimplifyError::Refused { inst, what },
    }
}

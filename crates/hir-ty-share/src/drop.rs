//! What a drop of a value reads: nothing when its type needs no drop, and otherwise the drop
//! glue it calls, which the code generator names by its type's full spelling (its
//! `InstanceDef::DropGlue` instance is keyed by its argument), so by the type's identity here.

use blaze_ids::ContentId;
use hir_ty::next_solver::Ty;

use crate::{Unshared, fact::Cx, identity};

/// The drop key of `ty`: the zero ID for a type with no drop glue, its identity otherwise.
/// The identity is of `ty` normalized all through, as the code generator names the glue's
/// instance (`places::evaluated` of its arguments): a place's type is a field's instantiated
/// with its ADT's arguments, which can hold a projection inside, such as addr2line's
/// `Load<<MappedLookup<T, L, {closure}> as LookupContinuation>::Buf>`.
///
/// Normalizing and spelling recurse once per level of `ty`, which a callee's argument makes as
/// deep as the program built it, so it runs with stack to spare (rule 20).
pub fn key<'db>(cx: &Cx<'db>, ty: Ty<'db>) -> Result<ContentId, Unshared> {
    stdx::ensure_sufficient_stack(|| {
        let ty = cx.normalize(ty);
        if !cx.needs_drop(ty) {
            return Ok(ContentId::default());
        }
        identity::ty(cx.db, cx.deeply_normalize(ty)?)
    })
}

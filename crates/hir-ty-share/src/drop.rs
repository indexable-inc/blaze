//! What a drop of a value reads: nothing when its type needs no drop, and otherwise the drop
//! glue it calls, which the code generator names by its type's full spelling (its
//! `InstanceDef::DropGlue` instance is keyed by its argument), so by the type's identity here.

use blaze_ids::ContentId;
use hir_ty::next_solver::Ty;

use crate::{Unshared, fact::Cx, identity};

/// The drop key of `ty`: the zero ID for a type with no drop glue, its identity otherwise.
pub(crate) fn key<'db>(cx: &Cx<'db>, ty: Ty<'db>) -> Result<ContentId, Unshared> {
    let ty = cx.normalize(ty);
    if !cx.needs_drop(ty) {
        return Ok(ContentId::default());
    }
    identity::ty(cx.db, ty)
}

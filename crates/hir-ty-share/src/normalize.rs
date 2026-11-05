//! A type with every alias in it normalized, for spelling it by content: the one way compile
//! identity and its building blocks normalize a type all through.

use hir_ty::next_solver::{
    ParamEnv, Ty,
    infer::{InferCtxt, traits::ObligationCause},
    references_non_lt_error,
};
use rustc_type_ir::TypeVisitableExt as _;

use crate::Unshared;

/// `ty` with every alias in it normalized in `param_env`, as rustc's `normalize_erasing_regions`
/// has a type before layout and codegen: a field instantiated with its ADT's arguments, or a
/// closure's signature, can hold a projection deep inside that only resolves with them.
///
/// - An alias left rigid where the generic body was checked is made normalizable first, as
///   `hir-ty-mir`'s `Filler::fill` does (rustc's `post_analysis_normalize`): its parameters are
///   filled now, so it may resolve.
/// - A result that holds an error (a constant whose evaluation failed normalizes to
///   `ConstKind::Error`) is refused with a reason naming `ty`, never spelled.
/// - `ty` must have no escaping bound variables (the solver's `deeply_normalize` asserts it):
///   normalize the type a binder belongs to (a function pointer, a `dyn`), never a part from inside
///   the binder alone. One that has them is refused here, naming it, before the solver's assert.
pub fn deeply_normalize<'db>(
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
) -> Result<Ty<'db>, Unshared> {
    if !ty.has_aliases() {
        return Ok(ty);
    }
    if ty.has_escaping_bound_vars() {
        return Err(Unshared::NoContent(format!(
            // alloc: error path
            "`{ty:?}` has bound variables of a binder outside it, so it is not normalized alone"
        )));
    }
    let reset = rustc_type_ir::set_aliases_to_non_rigid(infcx.interner, ty).skip_normalization();
    let normalized =
        infcx.at(&ObligationCause::dummy(), param_env).deeply_normalize(reset).map_err(
            |errors| Unshared::NoContent(format!("`{ty:?}` does not normalize: {errors:?}")),
        )?; // alloc: error path
    if references_non_lt_error(&normalized) {
        return Err(Unshared::NoContent(format!(
            // alloc: error path
            "`{ty:?}` normalizes to `{normalized:?}`, which holds an error (a constant in it that \
             does not evaluate)"
        )));
    }
    Ok(normalized)
}

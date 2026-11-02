//! rustc's `check_coroutine_obligations` (`rustc_hir_typeck`): the goals inference set aside as
//! stalled on a coroutine the body defines, proved between typeck and borrowck, once the
//! coroutines' witnesses (`coroutine_hidden_types`) are known.

use base_db::Db;
use hir_def::HasModule;
use rustc_type_ir::TypingMode;

use crate::{
    InferBodyId,
    db::HirDatabase,
    infer::{InferenceDiagnostic, InferenceResult},
    next_solver::{
        DbInterner,
        infer::{
            DbInternerInferExt,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    },
    solver_errors::{SolverDiagnostic, ambiguity_diagnostics},
};

/// The errors of [`InferenceResult::coroutine_stalled_predicates`]: each goal solved in
/// rustc's borrowck typing mode (`TypingMode::borrowck`, the opaques the body defines still
/// its own), where an ambiguity is an error too. The opaque uses solving adds are dropped
/// unchecked, as rustc drops them: no region constraint is checked here either. Empty, with no
/// inference context made, for a body whose inference set no goal aside.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn coroutine_obligation_errors<'db>(
    db: &'db Db,
    body: InferBodyId<'db>,
) -> Box<[InferenceDiagnostic]> {
    let stalled = &InferenceResult::of(db, body).coroutine_stalled_predicates;
    if stalled.is_empty() {
        return Box::default();
    }
    let generic_def = body.generic_def(db);
    let param_env = db.trait_environment(generic_def);
    let interner = DbInterner::new_with(db, generic_def.module(db).krate(db));
    let infcx = interner.infer_ctxt().build(TypingMode::borrowck(interner, body.into()));
    let mut ocx = ObligationCtxt::new(&infcx);
    for &(span, predicate) in stalled.iter() {
        ocx.register_obligation(Obligation::new(
            interner,
            ObligationCause::new(span),
            param_env,
            predicate.as_ref(),
        ));
    }
    let errors: Vec<_> = ocx
        .evaluate_obligations_error_on_ambiguity()
        .into_iter()
        .map(|error| error.into_fulfillment_error(&infcx))
        .collect(); // alloc: error path, empty when the goals hold
    _ = infcx.take_opaque_types();
    if errors.is_empty() {
        return Box::default();
    }
    let mut diagnostics: Vec<_> = errors
        .iter()
        .filter_map(|error| SolverDiagnostic::from_fulfillment_error(&infcx, error))
        .map(InferenceDiagnostic::SolverDiagnostic)
        .collect(); // alloc: error path
    diagnostics.extend(ambiguity_diagnostics(&infcx, errors, false));
    diagnostics.into_boxed_slice()
}

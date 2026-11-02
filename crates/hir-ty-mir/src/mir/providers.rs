//! The code of hir-ty's MIR queries (`hir_ty::providers`), registered once for every binary
//! that links this crate.

use base_db::Db;
use hir_ty::{
    mir::{MirBody, MirEvalError},
    next_solver::Allocation,
    providers::Mir,
    traits::ParamEnvAndCrate,
};

use super::{eval, lower, monomorphization, optimize, transform};

inventory::submit! {
    Mir {
        build: lower::build_mir_body,
        run_passes: transform::run_passes,
        optimize: optimize::optimize,
        monomorphize: monomorphization::monomorphized,

        interpret_mir,
        opaque_of_impl: monomorphization::opaque_of_impl,
        coroutine_witnesses: transform::compute_coroutine_witnesses,
        own_tests: cfg!(test),
    }
}

/// The value running `body` returns; what it printed is the interpreter's callers'.
fn interpret_mir<'db>(
    db: &'db Db,
    body: &'db MirBody<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, MirEvalError<'db>> {
    eval::interpret_mir(db, body, trait_env)
        .and_then(|(value, _)| value)
}

//! Moving results off the heap once a build has stored what it made of them (#806: the MIR of
//! every compiled body was 15% of a cold build's live heap; #734).
//!
//! Each value dropped keeps its memo, its fingerprint and the edges it read (the query database's
//! `evict_values`, which every query with a `Hash` output has): a dependent verifies against it without the value, and a
//! read computes it again and backdates by fingerprint, so a later edit cuts off early as if it
//! had been kept. What is dropped:
//!
//! - every MIR of every body, built, runtime, optimized and monomorphized, of closures and
//!   coroutines too: once code is stored, codegen does not read it again, and the borrow check
//!   and constant evaluation keep their own results;
//! - the inference results of the bodies given;
//! - the value of every constant and static evaluated: what codegen stored it as is read in
//!   its place, and a constant evaluation that needs one again computes it.
//!
//! The borrow check's results stay: they are small, and the live-patch and build gates
//! ([`hir_ty_borrowck::checked`]) read them on every build.

use base_db::Db;
use hir_def::DefWithBodyId;

use crate::mir;

/// After a build stored the code of `compiled`: drops every MIR, every constant's and
/// static's value and the inference of `compiled`. A workspace body `blaze check` reads again
/// is better left out of `compiled` when a check follows the build.
pub fn drop_after_store(db: &mut Db, compiled: &[DefWithBodyId]) {
    drop_mir(db);
    hir_ty::evict::drop_all_const_eval(db);
    hir_ty::evict::drop_inference(db, compiled);
}

/// Drops every MIR the database holds.
pub fn drop_mir(db: &mut Db) {
    mir::mir_body_query::evict_values(db);
    mir::mir_body_for_closure_query::evict_values(db);
    mir::mir_body_for_coroutine_query::evict_values(db);
    mir::mir_body_for_coroutine_by_move_query::evict_values(db);
    mir::promoted_mir_body_query::evict_values(db);

    mir::runtime_mir_body_query::evict_values(db);
    mir::runtime_mir_body_for_closure_query::evict_values(db);
    mir::runtime_mir_body_for_coroutine_query::evict_values(db);
    mir::runtime_mir_body_for_coroutine_by_move_query::evict_values(db);
    mir::runtime_promoted_mir_body_query::evict_values(db);

    mir::optimized_mir_body_query::evict_values(db);
    mir::optimized_mir_body_for_closure_query::evict_values(db);
    mir::optimized_mir_body_for_coroutine_query::evict_values(db);
    mir::optimized_mir_body_for_coroutine_by_move_query::evict_values(db);

    mir::monomorphized_mir_body_query::evict_values(db);
    mir::monomorphized_mir_body_for_closure_query::evict_values(db);
    mir::monomorphized_mir_body_for_coroutine_query::evict_values(db);
    mir::monomorphized_mir_body_for_coroutine_by_move_query::evict_values(db);
    mir::monomorphized_promoted_mir_body_query::evict_values(db);

    mir::monomorphized_runtime_mir_body_query::evict_values(db);
    mir::monomorphized_runtime_mir_body_for_closure_query::evict_values(db);
    mir::monomorphized_runtime_mir_body_for_coroutine_query::evict_values(db);
    mir::monomorphized_runtime_mir_body_for_coroutine_by_move_query::evict_values(db);

    mir::monomorphized_optimized_mir_body_query::evict_values(db);
    mir::monomorphized_optimized_mir_body_for_closure_query::evict_values(db);
    mir::monomorphized_optimized_mir_body_for_coroutine_query::evict_values(db);
    mir::monomorphized_optimized_mir_body_for_coroutine_by_move_query::evict_values(db);
}

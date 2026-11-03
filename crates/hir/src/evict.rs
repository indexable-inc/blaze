//! Moving results off the heap once a build has stored what it made of them (#806: the MIR of
//! every compiled body was 15% of a cold build's live heap; #734).
//!
//! Each value dropped keeps its memo, its fingerprint and the edges it read (the query database's
//! `evict_values`, which every query with a `Hash` output has): a dependent verifies against it without the value, and a
//! read computes it again and backdates by fingerprint, so a later edit cuts off early as if it
//! had been kept. What is dropped:
//!
//! - every MIR of every body, built, runtime, optimized, specialized and monomorphized, of closures and
//!   coroutines too: once code is stored, codegen does not read it again, and the borrow check
//!   and constant evaluation keep their own results;
//! - the inference results of the bodies given;
//! - the value of every constant and static evaluated: what codegen stored it as is read in
//!   its place, and a constant evaluation that needs one again computes it.
//!
//! The borrow check's results stay: they are small, and the live-patch and build gates
//! ([`hir_ty_borrowck::checked`]) read them on every build.

use base_db::{Crate, Db};
use hir_def::{DefWithBodyId, nameres::demand::crate_body_owners};

use crate::mir;

/// What [`drop_crates_after_store`] let go, by how many values: what its span and the release's
/// line say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The bodies of the crates given, whose inference was asked to go.
    pub bodies: usize,
    /// The inference results that went.
    pub inference: usize,
    /// The MIR bodies that went, of every function: none unless asked.
    pub mir: usize,
    /// The constants' and statics' values that went: none unless MIR went too.
    pub const_eval: usize,
    /// The trait solver's caches shared by a crate's queries that went: those of the crates
    /// given, and with MIR every crate's.
    pub solver_caches: usize,
    /// The interner indexes' grown-out tables freed, every one past its grace
    /// ([`hir_ty::evict::free_retired_intern_tables`]).
    pub intern_tables: usize,
}

/// Mid-build, once every unit of `crates` stored its code and nothing still to compile reads
/// their bodies (a dependent reads their metadata, or what the store keeps of them): drops the
/// inference of every body they declare outside a body ([`crate_body_owners`], which lowers
/// none: a body declared inside one is small and kept), and with `mir` every MIR and every
/// constant's and static's value, as [`drop_after_store`] does. `mir` drops the MIR of every
/// crate, not only these: a caller asks for it when no unit is between its phases or memory is
/// short, and what a unit still to compile reads of it is made again. A value read again is
/// made again and backdated by its fingerprint, so nothing read can differ. The trait solver's
/// caches shared by these crates' queries go too, and with `mir` every crate's
/// ([`hir_ty::evict::drop_solver_caches`]): they are of this revision only, and a goal asked
/// again is solved again. Last, the interner indexes' tables grown out of and past their grace
/// are freed.
pub fn drop_crates_after_store(db: &mut Db, crates: &[Crate], mir: bool) -> Dropped {
    let mut dropped = Dropped::default();
    for &krate in crates {
        let bodies = crate_body_owners(db, krate);
        dropped.bodies += bodies.len();
        dropped.inference += hir_ty::evict::drop_inference(db, &bodies);
    }
    dropped.solver_caches = hir_ty::evict::drop_solver_caches(db, crates, mir);
    if mir {
        dropped.mir = drop_mir(db);
        dropped.const_eval = hir_ty::evict::drop_all_const_eval(db);
    }
    dropped.intern_tables = hir_ty::evict::free_retired_intern_tables();
    dropped
}

/// After a build stored the code of `compiled`: drops every MIR, every constant's and
/// static's value and the inference of `compiled`. A workspace body `blaze check` reads again
/// is better left out of `compiled` when a check follows the build.
pub fn drop_after_store(db: &mut Db, compiled: &[DefWithBodyId]) {
    drop_mir(db);
    hir_ty::evict::drop_all_const_eval(db);
    hir_ty::evict::drop_inference(db, compiled);
}

/// Drops every MIR the database holds; returns how many.
pub fn drop_mir(db: &mut Db) -> usize {
    let mut dropped = 0;
    dropped += mir::mir_body_query::evict_values(db);
    dropped += mir::mir_body_for_closure_query::evict_values(db);
    dropped += mir::mir_body_for_coroutine_query::evict_values(db);
    dropped += mir::mir_body_for_coroutine_by_move_query::evict_values(db);
    dropped += mir::promoted_mir_body_query::evict_values(db);

    dropped += mir::runtime_mir_body_query::evict_values(db);
    dropped += mir::runtime_mir_body_for_closure_query::evict_values(db);
    dropped += mir::runtime_mir_body_for_coroutine_query::evict_values(db);
    dropped += mir::runtime_mir_body_for_coroutine_by_move_query::evict_values(db);
    dropped += mir::runtime_promoted_mir_body_query::evict_values(db);

    dropped += mir::optimized_mir_body_query::evict_values(db);
    dropped += mir::optimized_mir_body_for_closure_query::evict_values(db);
    dropped += mir::optimized_mir_body_for_coroutine_query::evict_values(db);
    dropped += mir::optimized_mir_body_for_coroutine_by_move_query::evict_values(db);
    dropped += mir::specialized_mir_body::evict_values(db);

    dropped += mir::monomorphized_mir_body_query::evict_values(db);
    dropped += mir::monomorphized_mir_body_for_closure_query::evict_values(db);
    dropped += mir::monomorphized_mir_body_for_coroutine_query::evict_values(db);
    dropped += mir::monomorphized_mir_body_for_coroutine_by_move_query::evict_values(db);
    dropped += mir::monomorphized_promoted_mir_body_query::evict_values(db);

    dropped += mir::monomorphized_runtime_mir_body_query::evict_values(db);
    dropped += mir::monomorphized_runtime_mir_body_for_closure_query::evict_values(db);
    dropped += mir::monomorphized_runtime_mir_body_for_coroutine_query::evict_values(db);
    dropped += mir::monomorphized_runtime_mir_body_for_coroutine_by_move_query::evict_values(db);

    dropped += mir::monomorphized_optimized_mir_body_query::evict_values(db);
    dropped += mir::monomorphized_optimized_mir_body_for_closure_query::evict_values(db);
    dropped += mir::monomorphized_optimized_mir_body_for_coroutine_query::evict_values(db);
    dropped += mir::monomorphized_optimized_mir_body_for_coroutine_by_move_query::evict_values(db);
    dropped
}

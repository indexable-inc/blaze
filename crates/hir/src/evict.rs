//! Moving results off the heap once a build has stored what it made of them (#806: the MIR of
//! every compiled body was 15% of a cold build's live heap; #734).
//!
//! Each value dropped keeps its memo, its fingerprint and the edges it read (the query database's
//! `evict_values`, which every query with a `Hash` output has): a dependent verifies against it without the value, and a
//! read computes it again and backdates by fingerprint, so a later edit cuts off early as if it
//! had been kept. What is dropped:
//!
//! - every MIR of every body, built, runtime, optimized, specialized and monomorphized, of closures and
//!   coroutines too, by its tag ([`drop_mir`]): once code is stored, codegen does not read it
//!   again, and the borrow check and constant evaluation keep their own results;
//! - the inference results of the bodies given;
//! - the value of every constant and static evaluated: what codegen stored it as is read in
//!   its place, and a constant evaluation that needs one again computes it.
//!
//! The borrow check's results stay: they are small, and the live-patch and build gates
//! ([`hir_ty_borrowck::checked`]) read them on every build.

use base_db::{Crate, Db};
use blaze_query::db::Database as _;
use hir_def::{DefWithBodyId, nameres::demand::crate_body_owners};

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

/// Drops every MIR the database holds; returns how many. Every MIR is the request's: a
/// function tagged `mem_tag = Mir` whose values go only when asked (`lru = 0`) is a MIR of a
/// body (built, runtime, optimized, specialized, monomorphized, promoted, of closures and
/// coroutines), or what one is made from (a coroutine's witnesses, a body's location
/// literals), and each is let go by its tag ([`blaze_query::db::Storage::evict_tag`]), so a
/// MIR query added later goes with the rest, never kept for want of a line here. A build lets
/// go of them all as it ends, a check's too (`blaze-cargo`'s release): only their memos stay,
/// for the next build to verify against.
pub fn drop_mir(db: &mut Db) -> usize {
    db.storage_mut().evict_tag(blaze_mem::Tag::Mir)
}

/// How many MIR values the database holds ([`drop_mir`]'s), from counters.
pub fn mir_held(db: &Db) -> usize {
    db.storage().values_of(blaze_mem::Tag::Mir)
}

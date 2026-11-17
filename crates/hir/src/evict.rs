//! Moving results off the heap once a build has stored what it made of them (#806: the MIR of
//! every compiled body was 15% of a cold build's live heap; #734).
//!
//! On the session memo (blaze-memo) a query's value lives in its home arena and goes when the
//! arena is dropped, never one by one: there is no eviction by tag, so a body's inference, its
//! MIR and every constant's value stay until their crate's arena is dropped
//! (docs/v2/plan/design/front-end-on-memo.md, rule 3: memory is bounded by arena drop). What is
//! still let go here is what hir-ty keeps beside the memo:
//!
//! - the trait solver's caches shared by a crate's queries;
//! - the interner indexes' tables grown out of and past their grace.

use base_db::{Crate, Db};
/// Counting the front ends a daemon keeps, and releasing the trait solver's interner tables once
/// none is (`hir_ty::evict::release_process_tables`).
pub use hir_ty::evict::{
    Released, front_end_dropped, front_end_kept, front_end_taken, release_process_tables,
    tables_released,
};

/// What [`drop_crates_after_store`] let go, by how many values: what its span and the release's
/// line say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The trait solver's caches shared by a crate's queries that went: those of the crates
    /// given, and with `all` every crate's.
    pub solver_caches: usize,
    /// The interner indexes' grown-out tables freed, every one past its grace
    /// ([`hir_ty::evict::free_retired_intern_tables`]).
    pub intern_tables: usize,
}

/// Mid-build, once every unit of `crates` stored its code and nothing still to compile reads
/// their bodies: drops the trait solver's caches shared by these crates' queries, and with `all`
/// every crate's ([`hir_ty::evict::drop_solver_caches`]): a goal asked again is solved again.
/// Then the interner indexes' tables grown out of and past their grace are freed. A body's
/// inference, MIR and constants are memos of its crate's arena and go with it, not here.
pub fn drop_crates_after_store(db: &Db, crates: &[Crate], all: bool) -> Dropped {
    Dropped {
        solver_caches: hir_ty::evict::drop_solver_caches(db, crates, all),
        intern_tables: hir_ty::evict::free_retired_intern_tables(),
    }
}

/// Lets go of every crate's trait solver caches and nothing else: how many went. For a
/// build's end that keeps its inference (blaze-cargo's release of an edit's build).
pub fn drop_every_solver_cache(db: &Db) -> usize {
    hir_ty::evict::drop_solver_caches(db, &[], true)
}

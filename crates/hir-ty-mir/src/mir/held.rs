//! How many MIR values the memo holds: the live entries of every query whose value is a MIR
//! body or what one is built from (the queries the old engine tagged `Tag::Mir`), over every
//! crate's arena and the session's. What a test that a walk keeps no body an instance counts
//! (hir-ty-pure's `tests/held.rs`, this crate's `tests/check_cache.rs`).

use base_db::{Db, all_crates};
use blaze_memo::{Database as _, Id as _};

/// The queries whose entries are MIR values, by the name blaze-memo gives a query: its
/// module's path and its name.
pub const MIR_QUERIES: &[&str] = &[
    "hir_ty::body_store::mir::location_literals",
    "hir_ty::providers::mir_body_query",
    "hir_ty::providers::mir_body_for_closure_query",
    "hir_ty::providers::mir_body_for_coroutine_query",
    "hir_ty::providers::mir_body_for_coroutine_by_move_query",
    "hir_ty::providers::monomorphized_mir_body_query",
    "hir_ty::providers::monomorphized_mir_body_for_closure_query",
    "hir_ty::providers::monomorphized_mir_body_for_coroutine_query",
    "hir_ty::providers::monomorphized_mir_body_for_coroutine_by_move_query",
    "hir_ty::providers::runtime_mir_body_query",
    "hir_ty::providers::runtime_mir_body_for_closure_query",
    "hir_ty::providers::runtime_mir_body_for_coroutine_query",
    "hir_ty::providers::runtime_mir_body_for_coroutine_by_move_query",
    "hir_ty::providers::monomorphized_runtime_mir_body_query",
    "hir_ty::providers::monomorphized_runtime_mir_body_for_closure_query",
    "hir_ty::providers::monomorphized_runtime_mir_body_for_coroutine_query",
    "hir_ty::providers::monomorphized_runtime_mir_body_for_coroutine_by_move_query",
    "hir_ty::providers::optimized_mir_body_query",
    "hir_ty::providers::optimized_mir_body_for_closure_query",
    "hir_ty::providers::optimized_mir_body_for_coroutine_query",
    "hir_ty::providers::optimized_mir_body_for_coroutine_by_move_query",
    "hir_ty::providers::monomorphized_optimized_mir_body_query",
    "hir_ty::providers::monomorphized_optimized_mir_body_for_closure_query",
    "hir_ty::providers::monomorphized_optimized_mir_body_for_coroutine_query",
    "hir_ty::providers::monomorphized_optimized_mir_body_for_coroutine_by_move_query",
    "hir_ty::providers::coroutine_witnesses_query",
    "hir_ty_mir::mir::lower::promoted_mir_body_query",
    "hir_ty_mir::mir::transform::runtime_promoted_mir_body_query",
    "hir_ty_mir::mir::monomorphization::monomorphized_promoted_mir_body_query",
    "hir_ty_mir::mir::optimize::specialize::specialized_mir_body",
];

/// The live entries of [`MIR_QUERIES`] in `db`'s memo, over the session's arena and every
/// crate's.
pub fn held(db: &Db) -> usize {
    let memo = db.memo();
    let all = all_crates(db);
    let crates = all.iter().map(|krate| krate.gid().arena());
    std::iter::once(memo.session())
        .chain(crates)
        .map(|arena| MIR_QUERIES.iter().map(|query| memo.entries(arena, query)).sum::<usize>())
        .sum()
}

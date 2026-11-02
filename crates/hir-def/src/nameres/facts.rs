//! Name resolution per module, with facts (`docs/v2/plan/lanes/frontend/nameres.md`).
//!
//! The old collector's rules, on a schedule that memoizes per module: each module's round-0
//! declarations are a query (`module_decls`) whose value is an op log; each expansion round
//! resolves imports per strongly connected component of the round's import graph
//! (`scc_state`), reading other components only through per-name facts (`ModuleScope::name`);
//! macro resolution is per module and round (`macro_step`); the crate's `DefMap` is assembled
//! from the final round's `ModuleScope` handles.

// The round schedule (`driver`, `graph`, the round queries) is the differential reference of
// the name resolution maintainer (`nameres::delta`), which reuses the rules through `collect`,
// `env`, `macros`, `ops` and `unit`.
pub(in crate::nameres) mod collect;
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::nameres) mod driver;
pub(in crate::nameres) mod env;
#[cfg_attr(not(test), allow(dead_code))]
mod graph;
pub(crate) mod impls;
pub mod kinds;
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::nameres) mod macros;
pub(in crate::nameres) mod ops;
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::nameres) mod queries;
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::nameres) mod unit;
#[cfg(test)]
pub(in crate::nameres) mod tests;

use base_db::{Crate, Db};
use blaze_hash::FixedState;
use blaze_mem::Frame;

use crate::{
    ModuleId,
    item_tree::TreeId,
    nameres::{DefMap, LocalDefMap, ModuleData},
    unstable_features::UnstableFeatures,
};

impl UnstableFeatures {
    /// The crate's `#![feature]`s as its collection reads them: the root file's item tree with
    /// its `cfg_attr`s applied, then the `-Z crate-attr` files. Equal to
    /// `crate_def_map(db, krate).features()` but never reads the def map, so a proc macro the
    /// crate calls while its map is being built can read them without a cycle.
    pub fn of_root(db: &Db, krate: Crate) -> &UnstableFeatures {
        &queries::crate_data(db, krate).unstable_features
    }
}

/// What a query keeps while it runs, in the frame its `frame::task` opened: all of it goes when
/// the query returns, and what it returns is built from it at its exact size.
pub(crate) type FrameVec<'f, T> = Vec<T, &'f Frame>;

pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FixedState, &'f Frame>;

pub(crate) fn map_in<'f, K, V>(frame: &'f Frame) -> FrameMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FixedState, frame)
}

pub(crate) type FrameSet<'f, K> = hashbrown::HashSet<K, FixedState, &'f Frame>;

pub(crate) fn set_in<'f, K>(frame: &'f Frame) -> FrameSet<'f, K> {
    hashbrown::HashSet::with_hasher_in(FixedState, frame)
}

/// The crate's def map with every scope handed over (`ScopeSlot::Collected`), diagnostics
/// grouped: what `crate_local_def_map` wraps in a `DefMapPair`. Runs inside that query only, in
/// its frame. The differential reference of the maintained resolution (`nameres::delta`).
#[cfg(test)]
pub(super) fn collect_crate(db: &Db, frame: &Frame, krate: Crate) -> (DefMap, LocalDefMap) {
    driver::collect_crate(db, frame, krate)
}

/// A block's def map from the empty one `block_def_map` makes, scopes handed over. Runs inside
/// that query only, in its frame.
pub(super) fn collect_block(
    db: &Db,
    frame: &Frame,
    def_map: DefMap,
    tree_id: TreeId,
    crate_local_def_map: &LocalDefMap,
) -> DefMap {
    driver::collect_block(db, frame, def_map, tree_id, crate_local_def_map)
}

/// The `ModuleScope` (as `blaze_query::db::Id`) of a `ScopeSlot::Fact(scope)`.
pub(super) fn fact_scope(db: &Db, scope: blaze_query::db::Id) -> blaze_query::db::Id {
    let scope = <queries::ScopeRef as blaze_query::db::FromId>::from_id(scope);
    queries::scope_of(db, scope)
}

/// The row of `module` in a resolver view (`ModulesSlot::Unit`) that does not hold it.
pub(super) fn row(db: &Db, rows: blaze_query::db::Id, module: ModuleId) -> Option<&ModuleData> {
    let rows = <queries::RowsKey as blaze_query::db::FromId>::from_id(rows);
    queries::row(db, rows, module).as_ref()
}

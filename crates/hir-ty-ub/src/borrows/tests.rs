//! Tests of Tree Borrows: Miri's unit tests of each part (`perms.rs`, `unimap.rs`,
//! `foreign_access_skipping.rs`, `dedup_range_map.rs`), its exhaustive tests of the state
//! machine (`tree/tests.rs`), tests of the API and the GC, and Miri's
//! `tests/{fail,pass}/tree_borrows/` programs as the same sequences of retags and accesses
//! with the same verdict.

mod api;
mod fail;
mod fail_wildcard;
mod pass;
mod perms;
mod range_map;
mod sim;
mod tree;
mod unimap;

//! The Heads stage plans no impl tables for a crate without a check scope (blaze-world
//! check_scope.rs `has_check_scope`, engine_heads.rs `plan`; rule 77): a crate the list refused
//! (a member proc macro on a shelf with no `wasm32-wasip1` side) or a dependency's proc macro
//! kept for its macros only (`clap_derive`, `serde_derive`, `thiserror_impl`: 2026-11-17, each
//! printed "heads: 1 head(s) failed: its crate's check scope was not read: its check scope
//! could not be made: the bodies of `clap_derive` are checked on the Macro side ..."). A child of
//! main.rs's tests, over engine_check_tests.rs's helpers.

use super::engine_check_tests::{failed_dep, said};
use super::*;

/// A REFUSED PROC MACRO HAS NO IMPL TABLES TASK: `p` (a member proc macro the test shelf cannot
/// build) fails its names, once, by name; no heads line says its check scope was not read or
/// could not be made, since no `ImplBucket` task is planned for it. CONTROL: `p` really failed
/// (its failure is printed). Before the fix its task was planned and refused, printing
/// "heads: 1 head(s) failed: its crate's check scope was not read: crate `p` has no names
/// record this command". The registry proc macro's row (the real projects' case) is the same
/// predicate's other arm, proven over the world and its crate list in blaze-world
/// check_scope_macro_tests.rs; no simulated home here fetches a registry package.
#[test]
fn a_crate_without_a_check_scope_is_planned_no_impl_tables() {
    let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
    failed_dep(&io);
    let (_, lines, checked) = said(&io, true);
    for line in lines.iter() {
        println!("{line}");
    }
    println!("impl tables (entered, failed, standing): {:?}", checked.heads);
    assert!(lines.iter().any(|line| line.contains("crate `p`")), "CONTROL: `p`'s names failed, naming it (else the fixture fails nothing)");
    assert!(!lines.iter().any(|line| line.contains("check scope could not be made")), "no crate's check scope is asked and refused");
    assert!(!lines.iter().any(|line| line.contains("its crate's check scope was not read: crate `p`")), "no impl tables task is planned for the refused `p`");
}

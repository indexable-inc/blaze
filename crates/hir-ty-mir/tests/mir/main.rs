//! The MIR passes over fixtures, through the queries a build asks: rustc's UB checks, storage
//! markers kept for a code generator that emits them, `file!()` as the build names files, and
//! that `Release` references nothing `Debug` doesn't. An integration test, so the test
//! database's queries are of the one copy of this crate the tests call.

mod alloc_budget;
mod const_check;
mod const_check_drops;
mod db;
mod erase_regions;
mod foreign;
mod held_across_calls;
#[path = "../../src/mir/optimize/inline/fixtures.rs"]
mod inline;
mod machine;
mod normalize;
mod operands;
mod references;
mod scalar_names;
mod source_maps;
mod source_names;
mod stages;
mod storage;
mod tail_call;
mod threads;
mod ub_checks;
mod uninhabited;
mod unwind;

use hir_def::{ConstId, FunctionId};
use hir_ty::db::HirDatabase;
use hir_ty_mir::mir::{MirOptions, print_mir_body};
use hir_expand::fixture;

use db::TestDB;

/// Every allocation of the test binary, counted per thread, for `alloc_budget`'s budgets.
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

#[track_caller]
fn constant(db: &TestDB, name: &str) -> ConstId {
    hir_def::test_items::constant(db, fixture::test_crate(db), name)
}

/// The MIR of the function `name` in a one-file fixture, as asked with `options`, printed.
#[track_caller]
fn mir(fixture: &str, name: &str, options: MirOptions) -> String {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, name);
        let body = db
            .optimized_mir_body(func.into(), options)
            .unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"));
        print_mir_body(&db, &body, name)
    })
}

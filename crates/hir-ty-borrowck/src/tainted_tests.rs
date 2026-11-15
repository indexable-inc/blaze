//! A body with an error reported before MIR is built is not borrow checked (rustc's
//! `tainted_by_errors`): its user sees that error once, never a second "cannot borrow-check"
//! from MIR lowering of what the error left meaningless.
//!
//! The invariant: a struct expression that leaves out a field, in a body or a closure in it, is
//! type checking's E0063, and the body's borrow check is [`NotChecked::TypeErrors`] with nothing
//! skipped and nothing reported; the same body with the field written is checked.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty::{InferBodyId, InferenceDiagnostic, InferenceResult};

use crate::{Mode, NotChecked, borrowck_in_mode, tainted_by_errors};

const FIXTURE: &str = r#"
//- minicore: fn
pub struct Config { pub jobs: u32, pub dir: u32 }
pub enum Ahead { Config { config: Config } }

fn direct() -> Config {
    Config { jobs: 1 }
}

fn in_a_closure() -> Ahead {
    let make = || Ahead::Config { config: Config { jobs: 1 } };
    make()
}

fn whole() -> Ahead {
    let make = || Ahead::Config { config: Config { jobs: 1, dir: 2 } };
    make()
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}`")
}

/// Each mode's check of `name`, and the number of E0063s its type check reported.
fn checked(name: &str) -> (Vec<crate::BorrowckResult>, usize, Option<NotChecked>) {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(function.into()));
        let missing = infer
            .diagnostics()
            .iter()
            .filter(|it| matches!(it, InferenceDiagnostic::RecordMissingFields { .. }))
            .count();
        let results = [Mode::Nll, Mode::Polonius, Mode::PoloniusFlow]
            .map(|mode| borrowck_in_mode(&db, function.into(), mode));
        // alloc: a test's results
        (results.into(), missing, tainted_by_errors(&db, function.into()))
    })
}

#[test]
fn a_missing_field_is_e0063_and_the_body_is_not_borrow_checked() {
    for name in ["direct", "in_a_closure"] {
        let (results, missing, tainted) = checked(name);
        assert_eq!(missing, 1, "`{name}`: one E0063 from type checking");
        assert_eq!(tainted, Some(NotChecked::TypeErrors), "`{name}`");
        for result in results {
            assert_eq!(result.not_checked, Some(NotChecked::TypeErrors), "`{name}`");
            assert_eq!(result.skipped, None, "`{name}`: nothing for MIR lowering to refuse");
            assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
        }
    }
}

/// The control: with the field written the body is checked, and passes.
#[test]
fn the_same_body_with_every_field_is_borrow_checked() {
    let (results, missing, tainted) = checked("whole");
    assert_eq!(missing, 0);
    assert_eq!(tainted, None);
    for result in results {
        assert_eq!(result.not_checked, None);
        assert_eq!(result.skipped, None);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }
}

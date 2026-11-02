//! A signature that leaves out a lifetime with none to elide to (E0106) has an error region,
//! and the body that meets one, its own or a callee's, gets no lifetime error: its outlives
//! constraints and type tests are dropped. Reduced from `blaze-link`'s `cow.rs` `unique`: one
//! parameter with two lifetimes, the elided `&mut` and the `'static` written in it.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_ty::{MissingLifetimeKind, TyLoweringDiagnostic, db::HirDatabase as _};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};


const FIXTURE: &str = r#"
//- minicore: sized, copy
static ZERO: u8 = 0;

struct Shared<'p, T> {
    pool: &'p u8,
    value: T,
}

impl<'p, T> Shared<'p, T> {
    fn get_mut(this: &mut Self) -> &mut T {
        &mut this.value
    }
}

fn unique(shared: &mut Shared<'static, u8>) -> &mut u8 {
    *shared = Shared { pool: &ZERO, value: 0 };
    Shared::<'static, u8>::get_mut(shared)
}

fn own(chunk: &mut Shared<'static, u8>) -> &mut u8 {
    unique(chunk)
}

fn unique_named<'s>(shared: &'s mut Shared<'static, u8>) -> &'s mut u8 {
    *shared = Shared { pool: &ZERO, value: 0 };
    Shared::<'static, u8>::get_mut(shared)
}

fn own_named<'s>(chunk: &'s mut Shared<'static, u8>) -> &'s mut u8 {
    unique_named(chunk)
}

fn only_static(x: &'static u8) -> &u8 {
    x
}

fn too_long<'s>(shared: &'s mut Shared<'static, u8>) -> &'static mut u8 {
    Shared::<'static, u8>::get_mut(shared)
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

/// Whether `name`'s signature fails lifetime elision, and its borrow check in each mode.
fn checked(name: &str) -> (bool, [BorrowckResult; 2]) {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let function = function(&db, name);
        let elision_failure =
            db.fn_sig_for_fn_with_diagnostics(function).diagnostics().iter().any(|it| {
                matches!(
                    it,
                    TyLoweringDiagnostic::MissingLifetimes {
                        kind: MissingLifetimeKind::ElisionFailure,
                        ..
                    }
                )
            });
        let results = [Mode::Nll, Mode::Polonius]
            .map(|mode| borrowck_in_mode(&db, function.into(), mode));
        (elision_failure, results)
    })
}

/// `name` is borrow checked, with no error.
fn accepted(results: &[BorrowckResult; 2], name: &str) {
    for result in results {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
    }
}

#[test]
fn an_elided_return_lifetime_beside_a_written_static_fails_elision_alone() {
    for name in ["unique", "own"] {
        let (elision_failure, results) = checked(name);
        assert!(elision_failure, "`{name}` elides its return lifetime");
        accepted(&results, name);
    }
}

#[test]
fn the_lifetime_written_out_is_accepted() {
    for name in ["unique_named", "own_named", "only_static"] {
        let (elision_failure, results) = checked(name);
        assert!(!elision_failure, "`{name}`");
        accepted(&results, name);
    }
}

#[test]
fn a_return_that_outlives_the_borrow_is_still_an_error() {
    let (elision_failure, results) = checked("too_long");
    assert!(!elision_failure);
    for result in &results {
        assert_eq!(result.not_checked, None);
        let messages: Vec<&str> = result.diagnostics.iter().map(|it| it.message.as_str()).collect();
        assert_eq!(messages, ["lifetime may not live long enough"]);
    }
}

//! A body whose lowered root is `Expr::Missing`: nothing to check when the source has no body (a
//! foreign fn), a refusal when it has one that was lowered to nothing (`#[rust_analyzer::skip]`,
//! which rustc ignores and checks the body of), never a clean check of what is not there.

use base_db::{Db, all_crates};
use hir_def::{
    AssocItemId, FunctionId, ModuleDefId,
    nameres::{assoc::TraitItems, crate_def_map},
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized, future
unsafe extern "C" {
    fn foreign() -> u8;
}

trait Tr {
    async fn required();
}

#[rust_analyzer::skip]
fn skipped() -> u8 {
    1
}

fn plain() -> u8 {
    1
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
                if let ModuleDefId::TraitId(it) = def {
                    let (items, _) = TraitItems::query_with_diagnostics(db, it);
                    for &(_, item) in items.items.iter() {
                        if let AssocItemId::FunctionId(it) = item
                            && FunctionSignature::of(db, it).name.as_str() == name
                        {
                            return it;
                        }
                    }
                }
            }
        }
    }
    panic!("no function `{name}`")
}

fn checked(name: &str) -> [BorrowckResult; 2] {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius].map(|mode| borrowck_in_mode(&db, function.into(), mode))
    })
}

#[test]
fn a_fn_without_a_body_has_nothing_to_check() {
    for result in checked("foreign") {
        assert_eq!(result.not_checked, None);
        assert_eq!(result.skipped, None);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }
}

/// A bodiless `async fn` lowers to an `async` block around nothing: no coroutine is checked.
#[test]
fn an_async_fn_without_a_body_has_nothing_to_check() {
    for result in checked("required") {
        assert_eq!(result.not_checked, None);
        assert_eq!(result.skipped, None);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }
}

#[test]
fn a_written_body_lowered_to_nothing_is_refused() {
    for result in checked("skipped") {
        let skipped = result.skipped.expect("a body that is written is not passed unchecked");
        assert!(skipped.contains("lowered to no expression"), "{skipped}");
    }
}

#[test]
fn a_body_is_checked() {
    for result in checked("plain") {
        assert_eq!(result.not_checked, None);
        assert_eq!(result.skipped, None);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }
}

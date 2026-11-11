//! Run-time contract checks as the borrow checker sees them (docs/v2/plan/design/contracts.md,
//! part 1): an `ensures` whose patterns bind lowers, so its function is borrow checked rather
//! than skipped (`InaccessibleLocal`), and reading the result by reference is no error.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{Mode, borrowck_in_mode};

const SOURCE: &str = r#"//- minicore: sized, copy, option, result
//- /main.rs cfg:blaze::contract_checks
#![feature(blaze_verify, lang_items)]
#[lang = "panic_nounwind"]
fn panic_nounwind(_: &'static str) -> ! { loop {} }
struct Key(u32);
fn key(n: u32) -> (r: Result<Key, u32>)
    ensures match &r {
        Ok(k) => k.0 < 10,
        Err(e) => *e == n,
    }
{
    if n > 100 { return Err(n); }
    Ok(Key(n))
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

#[test]
fn an_ensures_that_binds_is_borrow_checked_clean() {
    let fx = fixture::db(SOURCE);
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, "key");
        for mode in [Mode::Nll, Mode::Polonius] {
            let result = borrowck_in_mode(&db, function.into(), mode);
            assert_eq!(result.skipped, None, "`key` in {mode:?}");
            assert_eq!(result.not_checked, None, "`key` in {mode:?}");
            let errors: Vec<_> =
                result.diagnostics.iter().filter(|it| it.is_error()).map(|it| it.code).collect();
            assert!(errors.is_empty(), "`key` in {mode:?}: {errors:?}");
        }
    })
}

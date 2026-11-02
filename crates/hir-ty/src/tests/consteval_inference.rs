//! The constants `hir-ty-mir`'s consteval tests evaluate infer with no error: since a body whose
//! inference has errors evaluates to `HasErrors` (`consteval::hir_eval::value`), `loops`,
//! `options` and `ranges` refuse their `GOAL`. On a failure the message has, per body, the
//! error flag, the diagnostics and each node whose type has an error.

use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map};
use hir_expand::fixture;
use rustc_type_ir::TypeVisitableExt;

use crate::{InferBodyId, InferenceResult};

const FIXTURES: &[(&str, &str)] = &[
    (
        "loops: labeled loops",
        r#"
//- minicore: add, builtin_impls
const GOAL: u8 = {
    let mut x = 0;
    loop {
        x = x + 1;
        while true {
            break;
        }
        x = x + 1;
        if x == 2 {
            continue;
        }
        break;
    }
    x
};
"#,
    ),
    (
        "loops: break with a value",
        r#"
//- minicore: add, builtin_impls
const GOAL: u8 = {
    let mut x = 0;
    loop {
        x = x + 1;
        if x == 5 {
            break x + 2;
        }
    }
};
"#,
    ),
    (
        "loops: break to an outer label",
        r#"
//- minicore: add, builtin_impls
const GOAL: u8 = {
    'a: loop {
        let x: u8 = 'b: loop {
            let x = 'c: loop {
                let x = 'd: loop {
                    let x: u8 = 'e: loop {
                        break 'd 1;
                    };
                    break 2 + x;
                };
                break 3 + x;
            };
            break 'a 4 + x;
        };
        break 5 + x;
    }
};
"#,
    ),
    (
        "loops: while with compound assignment",
        r#"
//- minicore: add, builtin_impls
const GOAL: u8 = {
    let mut x = 0;
    'a: loop {
        'b: loop {
            'c: while x < 20 {
                'd: while x < 5 {
                    'e: loop {
                        x += 1;
                        continue 'c;
                    };
                };
                x += 1;
            };
            break 'a;
        };
    }
    x
};
"#,
    ),
    (
        "options: match on Some",
        r#"
//- minicore: option, add, builtin_impls
const GOAL: u8 = {
    let x = Some(2);
    match x {
        Some(y) => 2 + y,
        _ => 10,
    }
};
"#,
    ),
    (
        "options: let chains",
        r#"
//- minicore: option, add, builtin_impls
fn f(x: Option<Option<i32>>) -> i32 {
    if let Some(y) = x && let Some(z) = y {
        z
    } else if let Some(y) = x {
        1
    } else {
        0
    }
}
const GOAL: i32 = f(Some(Some(10))) + f(Some(None)) + f(None);
"#,
    ),
    (
        "options: match on None",
        r#"
//- minicore: option, add, builtin_impls
const GOAL: u8 = {
    let x: Option<u8> = None;
    match x {
        Some(y) => 2 + y,
        _ => 10,
    }
};
"#,
    ),
    (
        "ranges",
        r#"
//- minicore: range
const GOAL: i32 = (1..2).start + (20..10).end + (10000..).start + (..100000).end
    + (..=1000000).end;
"#,
    ),
    (
        "extern_weak_statics",
        r#"
//- minicore: sized
extern "C" {
    #[linkage = "extern_weak"]
    static __dso_handle: *mut u8;
}
const GOAL: usize = __dso_handle as usize;
"#,
    ),
];

#[test]
fn the_consteval_constants_infer_with_no_error() {
    let mut failed = String::new();
    for (what, text) in FIXTURES {
        let db = fixture::db(text).into_db();
        crate::next_solver::attach_db(&db, || {
            let mut owners = Vec::new();
            for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
                for def in module.scope(&db).declarations() {
                    match def {
                        ModuleDefId::ConstId(it) => owners.push(DefWithBodyId::ConstId(it)),
                        ModuleDefId::FunctionId(it) => owners.push(DefWithBodyId::FunctionId(it)),
                        _ => {}
                    }
                }
            }
            assert!(!owners.is_empty(), "{what}: no constant or function");
            for def in owners {
                let owner = InferBodyId::DefWithBodyId(def);
                let infer = InferenceResult::of(&db, owner);
                if !infer.has_errors && infer.diagnostics().is_empty() {
                    continue;
                }
                failed.push_str(&format!(
                    "{what}: {def:?}: has_errors {}, mismatches {}, diagnostics {:?}\n",
                    infer.has_errors,
                    infer.has_type_mismatches(),
                    infer.diagnostics(),
                ));
                let (store, _) = owner.store_and_root_expr(&db);
                for (id, expr) in store.exprs() {
                    let ty = infer.expr_ty(id);
                    if ty.references_error() {
                        failed.push_str(&format!("  {id:?} {expr:?}: {ty:?}\n"));
                    }
                }
            }
        });
    }
    assert!(failed.is_empty(), "{failed}");
}

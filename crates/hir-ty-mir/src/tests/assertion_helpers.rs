//! A fixture's own assertion helper asserts.
//!
//! A program a test runs on the evaluator checks itself with a function of its fixture:
//! `fn should_not_reach() { .. }` called where a wrong value was found. The test passes when
//! the program runs to its end, so a helper whose call returns makes every check through it
//! pass whatever the values are, and nothing says so: for a while the 251 calls of
//! `fn should_not_reach() { _ }` in these tests asserted nothing, because the typed hole was
//! lowered to nothing (agents row `lang1100-1`).
//!
//! So the helpers are held here, for every fixture the evaluator's tests run
//! (`tests::mir::eval`'s `eval_main`, `tests::consteval`'s `eval_goal`), before its program
//! runs:
//!
//! - a function of the fixture named as what must not happen (`should_not_..`,
//!   `unreachable..`) and taking nothing is called, and the call must not return;
//! - a function named as a check (`check`, `assert..`) that takes arguments must have a place
//!   that fails: a call of type `!` (a panic), or a typed hole, which does not lower.

use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, expr_store::Body, hir::Expr,
    signatures::FunctionSignature,
};
use hir_expand::EditionedFileId;
use span::Edition;

use crate::{
    InferBodyId, InferenceResult,
    db::HirDatabase as _,
    mir::interpret_mir,
    next_solver::GenericArgs,
    test_db::{TestDB, TestDBExt as _},
};

/// What a helper's name says it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Helper {
    /// Reaching it is the failure.
    MustNotReturn,
    /// It fails when what it is given does not hold.
    Check,
}

fn helper(name: &str, params: usize) -> Option<Helper> {
    let never = name.starts_with("should_not_") || name.starts_with("unreachable");
    let check = name == "check" || name.starts_with("assert");
    match (never, check, params) {
        (true, _, 0) => Some(Helper::MustNotReturn),
        (true, _, _) | (_, true, 1..) => Some(Helper::Check),
        _ => None,
    }
}

/// Panics, naming it, for a helper of `file_id`'s module that asserts nothing.
pub(crate) fn hold(db: &TestDB, file_id: EditionedFileId) {
    blaze_query::db::attach(db, || {
        let Some(module) = db.module_for_file_opt(file_id.file_id(db)) else { return };
        let def_map = module.def_map(db);
        let functions =
            def_map.module(db, module).scope(db).declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            });
        for function in functions {
            let signature = FunctionSignature::of(db, function);
            let name = signature.name.display(db, Edition::CURRENT).to_string();
            match helper(&name, signature.params.len()) {
                Some(Helper::MustNotReturn) => assert!(
                    !returns(db, function),
                    "the fixture's `{name}()` returns when it is called, so a check that calls \
                     it asserts nothing: make it panic",
                ),
                Some(Helper::Check) => assert!(
                    may_diverge(db, function),
                    "the fixture's `{name}` has no place that diverges (a panic), so it asserts \
                     nothing",
                ),
                None => {}
            }
        }
    })
}

/// Whether a call of `function`, which takes nothing, comes back.
fn returns(db: &TestDB, function: FunctionId) -> bool {
    let env = crate::ParamEnvAndCrate {
        param_env: db.trait_environment(function.into()),
        krate: hir_def::HasModule::krate(&function, db),
    }
    .store();
    let args = GenericArgs::empty().store();
    let Ok(body) = db.monomorphized_runtime_mir_body(function.into(), args, env) else {
        // It does not lower: a call of it stops the program.
        return false;
    };
    matches!(interpret_mir(db, body, None), Ok((Ok(_), _)))
}

/// Whether the body of `function` has an expression that does not come back.
fn may_diverge(db: &TestDB, function: FunctionId) -> bool {
    let def = DefWithBodyId::FunctionId(function);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, InferBodyId::DefWithBodyId(def));
    body.exprs().any(|(id, expr)| match expr {
        Expr::Underscore => true,
        // A call that does not come back (`panic!`'s). Leaving the helper (`return`, and the
        // blocks around one, which are of type `!` too) is not failing, nor is a loop.
        Expr::Call { .. } | Expr::MethodCall { .. } => infer.expr_ty(id).is_never(),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use hir_expand::fixture;

    use super::*;

    fn held(source: &str) -> bool {
        let fx = fixture::db(source);
        let file = *fx.files().last().unwrap();
        let db = fx.into_db();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hold(&db, file))).is_ok()
    }

    /// The control: a helper that returns is refused, whatever it is made of.
    #[test]
    fn the_oracle_catches_a_helper_that_returns() {
        assert!(!held("//- minicore: sized\nfn should_not_reach() {}\nfn main() {}"));
        assert!(!held("//- minicore: sized\nfn should_not_reach() { let _x = 1; }\nfn main() {}"));
        assert!(!held(
            "//- minicore: sized\nfn check(b: bool) { if !b { return; } }\nfn main() { check(true) }"
        ));
        assert!(!held("//- minicore: sized\nfn assert_eq(a: u8, b: u8) {}\nfn main() {}"));
    }

    #[test]
    fn a_helper_that_stops_the_program_is_held() {
        // A typed hole does not lower, so a call of it fails: the helper of most fixtures.
        assert!(held("//- minicore: sized\nfn should_not_reach() { _ }\nfn main() {}"));
        assert!(held(
            "//- minicore: panic\nfn should_not_reach() { panic!(\"no\") }\nfn main() {}"
        ));
        assert!(held(
            "//- minicore: panic\nfn check(b: bool) { if !b { panic!(\"no\") } }\nfn main() {}"
        ));
        // Not named as a helper.
        assert!(held("//- minicore: sized\nfn checked() {}\nfn main() {}"));
    }
}

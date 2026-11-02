//! `InferenceResult::expr_ty_after_adjustments`, which inference and MIR lowering both ask,
//! gives what each of their copies gave: the last adjustment's target, a trailing `NeverToAny`
//! included, or the expression's own type.

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, expr_store::Body, hir::ExprId,
    nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{Adjust, InferBodyId, InferenceResult, next_solver::Ty};

const FIXTURE: &str = r#"
//- minicore: deref, sized
struct S(u8);
impl S {
    fn get(&self) -> u8 { self.0 }
}
fn f(s: &&S, b: bool) -> u8 {
    let n = s.get();
    let m: u8 = if b { return n } else { 1 };
    let r: &u8 = &&m;
    *r + n
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
    panic!("no function `{name}` in the fixture")
}

/// The copies inference and MIR lowering had, over the frozen result.
fn reference<'db>(infer: &InferenceResult<'db>, e: ExprId) -> Ty<'db> {
    let mut ty = None;
    if let Some(it) = infer.expr_adjustments.get(&e)
        && let Some(it) = it.last()
    {
        ty = Some(it.target.as_ref());
    }
    ty.unwrap_or_else(|| infer.expr_ty(e))
}

#[test]
fn an_expressions_adjusted_type_is_what_both_old_copies_said() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, "f"));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        let (mut adjusted, mut never) = (0, 0);
        for (e, it) in body.exprs() {
            assert_eq!(infer.expr_ty_after_adjustments(e), reference(infer, e), "{it:?}");
            if let Some(list) = infer.expr_adjustments.get(&e)
                && let Some(last) = list.last()
            {
                adjusted += 1;
                never += usize::from(matches!(last.kind, Adjust::NeverToAny));
            }
        }
        assert!(adjusted >= 2, "the fixture autoderefs a receiver and a `&&u8`");
        assert!(never >= 1, "the fixture's `return` is coerced from `!`");
    });
}

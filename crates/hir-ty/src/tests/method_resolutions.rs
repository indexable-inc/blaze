//! Every method call and overloaded call records the function it calls, with the receiver's
//! type as the first argument: `s.area()` of `s: &dyn Shape` is `<dyn Shape as Shape>::area`,
//! `f(1)` of `f: &dyn Fn(u32) -> u32` is `<dyn Fn(u32) -> u32 as Fn>::call` (queue 01277:
//! blaze-lints' `hot_dyn` found only `Shape::area(s)`).

use hir_def::{
    DefWithBodyId, ModuleDefId,
    expr_store::Body,
    hir::Expr,
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use rustc_type_ir::inherent::IntoKind as _;

use crate::{InferBodyId, InferenceResult, next_solver::TyKind};

const FIXTURE: &str = r#"
//- minicore: sized, fn, dispatch_from_dyn
trait Shape {
    fn area(&self) -> u32;
}
fn hot(s: &dyn Shape, f: &dyn Fn(u32) -> u32, g: &mut dyn FnMut()) {
    s.area();
    f(1);
    g();
    Shape::area(s);
}
"#;

#[test]
fn calls_on_dyn_receivers_record_their_method() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let hot = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it) if FunctionSignature::of(&db, it).name.as_str() == "hot" => Some(it),
                _ => None,
            })
            .expect("the fixture's `hot`");
        let def = DefWithBodyId::FunctionId(hot);
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        let mut resolved = 0;
        for (id, expr) in body.exprs() {
            match expr {
                Expr::MethodCall { .. } => {}
                Expr::Call { callee, .. } if !matches!(infer.expr_ty(callee).kind(), TyKind::FnDef(..)) => {}
                _ => continue,
            }
            let (_, args) = infer.method_resolution(id).unwrap_or_else(|| panic!("{expr:?} has no method"));
            let mut self_ty = args.types().next().unwrap_or_else(|| panic!("{expr:?} has no `Self`"));
            while let TyKind::Ref(_, inner, _) = self_ty.kind() {
                self_ty = inner;
            }
            assert!(matches!(self_ty.kind(), TyKind::Dynamic(..)), "{expr:?} has `Self` {self_ty:?}");
            resolved += 1;
        }
        assert_eq!(resolved, 3, "`s.area()`, `f(1)` and `g()`");
    });
}

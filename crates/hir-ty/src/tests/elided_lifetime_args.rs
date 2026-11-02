//! rustc's rule for a path that gives type arguments but no lifetime arguments: every lifetime
//! is inferred and the type arguments start at the first type parameter, so
//! `S::<Mode>::size()` of `struct S<'a, 'b, M>` is `S<'_, '_, Mode>` (queue 01099: the
//! bootstrap gave it `S<'{region error}, '{region error}, {type error}>`).

use hir_def::{
    DefWithBodyId, ModuleDefId,
    expr_store::Body,
    hir::{Expr, ExprOrPatId},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use rustc_type_ir::{TypeVisitableExt, inherent::IntoKind as _};

use crate::{InferBodyId, InferenceResult, next_solver::TyKind};

const FIXTURE: &str = r#"
//- minicore: sized
struct S<'a, 'b, M>(&'a (), &'b (), M);
struct Mode;
impl<'a, 'b, M> S<'a, 'b, M> {
    fn size() -> u32 {
        0
    }
}
fn main() {
    S::<Mode>::size();
}
"#;

#[test]
fn type_args_with_no_lifetime_args_infer_the_lifetimes() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let main = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it) if FunctionSignature::of(&db, it).name.as_str() == "main" => Some(it),
                _ => None,
            })
            .expect("the fixture's `main`");
        let def = DefWithBodyId::FunctionId(main);
        let body = Body::of(&db, def);
        let path = body
            .exprs()
            .find(|(_, it)| matches!(it, Expr::Path(_)))
            .map(|(id, _)| id)
            .expect("the call's path");
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        let ty = infer.type_of_expr_or_pat(ExprOrPatId::ExprId(path)).expect("the path's type");
        // Writeback makes every region variable `'{region error}`, inferred ones too.
        let TyKind::FnDef(_, args) = ty.kind() else { panic!("`S::<Mode>::size` is {ty:?}") };
        let args = args.skip_binder();
        assert_eq!(args.len(), 3, "`S::<Mode>::size` is {ty:?}");
        assert!(args.types().all(|it| !it.references_error()), "`S::<Mode>::size` is {ty:?}");
        assert!(args.types().next().is_some_and(|it| it.as_adt().is_some()), "`S::<Mode>::size` is {ty:?}");
    });
}

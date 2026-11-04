//! A closure's `for<'a, ..>` binder (`#![feature(closure_lifetime_binder)]`, rustc's
//! `ClosureBinder::For`): its lifetimes are the late-bound ones of the closure's signature,
//! which its parameters and return type name. Before, the binder was not lowered and each
//! of them was `'{region error}`.

use hir_def::{
    DefWithBodyId, ModuleDefId,
    expr_store::Body,
    hir::{Expr, ExprOrPatId},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use rustc_type_ir::{
    BoundVarIndexKind, RegionKind,
    inherent::{GenericArgs as _, IntoKind as _},
};

use crate::{
    InferBodyId, InferenceResult,
    next_solver::{DbInterner, Ty, TyKind},
};

const FIXTURE: &str = r#"
//- minicore: sized, fn
#![feature(closure_lifetime_binder)]
fn main() {
    let _c = for<'a, 'c> |_: &'a &'c (), v: &'c u8| -> &'a u8 { v };
}
"#;

/// The signature's own variable of each region of `ty`, in order; `None` for any other region.
fn bound_vars<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Vec<Option<u32>> {
    let mut vars = Vec::new();
    rustc_type_ir::fold_regions(interner, ty, |region, depth| {
        vars.push(match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), bound) if debruijn == depth => {
                Some(bound.var.as_u32())
            }
            _ => None,
        });
        region
    });
    vars
}

#[test]
fn a_closures_binder_gives_its_signature_its_late_bound_lifetimes() {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let main = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it)
                    if FunctionSignature::of(&db, it).name.as_str() == "main" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `main`");
        let def = DefWithBodyId::FunctionId(main);
        let body = Body::of(&db, def);
        let closure = body
            .exprs()
            .find(|(_, it)| matches!(it, Expr::Closure { .. }))
            .map(|(id, _)| id)
            .expect("the closure");
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        let ty =
            infer.type_of_expr_or_pat(ExprOrPatId::ExprId(closure)).expect("the closure's type");
        let TyKind::Closure(_, args) = ty.kind() else { panic!("the closure is {ty:?}") };
        let sig = args.as_closure().sig();
        // `'a` and `'c`, and no lifetime left out.
        assert_eq!(sig.bound_vars().len(), 2, "{sig:?}");
        let sig = sig.skip_binder();
        let &[tupled] = sig.inputs() else { panic!("{sig:?}") };
        let TyKind::Tuple(inputs) = tupled.kind() else { panic!("{sig:?}") };
        let interner = DbInterner::new_no_crate(&db);
        // `&'a &'c ()`, `&'c u8` and `-> &'a u8`: one variable for each name, wherever it is.
        assert_eq!(bound_vars(interner, inputs.as_slice()[0]), [Some(0), Some(1)], "{sig:?}");
        assert_eq!(bound_vars(interner, inputs.as_slice()[1]), [Some(1)], "{sig:?}");
        assert_eq!(bound_vars(interner, sig.output()), [Some(0)], "{sig:?}");
    });
}

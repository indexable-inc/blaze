//! A layout blaze cannot compute is an error naming why, never a guess the build goes on with.

use hir_def::{AdtId, EnumId, GenericDefId, ModuleDefId, nameres::crate_def_map};
use hir_expand::fixture;
use rustc_type_ir::inherent::GenericArgs as _;

use crate::{
    ParamEnvAndCrate,
    db::HirDatabase,
    layout::{LayoutError, enum_variant_ids},
    next_solver::{DbInterner, GenericArgs, Ty},
};

#[test]
fn a_discriminant_that_does_not_evaluate_fails_the_layout() {
    let db = fixture::db(
        r#"
enum Goal {
    A = 0,
    B = 1 / 0,
}
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let krate = fixture::test_crate(&db);
        let goal: EnumId = crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::EnumId(it)) => Some(it),
                _ => None,
            })
            .expect("the fixture's one enum");
        let interner = DbInterner::new_no_crate(&db);
        let adt = AdtId::EnumId(goal);
        let ty = Ty::new_adt(interner, adt, GenericArgs::identity_for_item(interner, adt.into()));
        let param_env = db.trait_environment(GenericDefId::AdtId(adt));
        let layout = db.layout_of_ty(ty.store(), ParamEnvAndCrate { param_env, krate }.store());
        assert_eq!(
            layout.err(),
            Some(LayoutError::DiscriminantFailed { variant: enum_variant_ids(&db, goal)[1] }),
            "`B`'s discriminant divides by zero: the layout fails on it, it is not left out",
        );
    });
}

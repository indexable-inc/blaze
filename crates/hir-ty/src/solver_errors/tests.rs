use base_db::Db;
use hir_def::{ImplId, nameres::crate_def_map, signatures::TraitSignature};
use rustc_type_ir::{BoundConstness, TypingMode};
use hir_expand::fixture;

use super::non_const_impl;
use crate::{
    db::HirDatabase,
    next_solver::{
        AnyImplId, Binder, DbInterner, ParamEnv,
        infer::{
            DbInternerInferExt,
            traits::{Obligation, ObligationCause},
        },
    },
};

/// The impl of the trait named `trait_name`; the fixture has one.
fn impl_of(db: &Db, trait_name: &str) -> ImplId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .find(|&impl_| {
            db.impl_trait(impl_).is_some_and(|it| {
                TraitSignature::of(db, it.skip_binder().def_id.0).name.as_str() == trait_name
            })
        })
        .unwrap_or_else(|| panic!("no impl of `{trait_name}` in the fixture"))
}

/// rustc's `report_host_effect_error`: a `[const] Trait` goal met by an impl that is not `const`
/// names that impl ("trait `Tr` is implemented but not `const`"); a `const` impl names none.
#[test]
fn a_const_goal_names_the_impl_that_is_not_const() {
    let db = fixture::db(
        r#"
//- minicore: sized
#![feature(const_trait_impl)]
const trait Plain {}
const trait Const {}
impl Plain for u8 {}
impl const Const for u8 {}
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let named = |trait_name| {
            let impl_ = impl_of(&db, trait_name);
            let trait_ref = db.impl_trait(impl_).unwrap().skip_binder();
            let clause =
                Binder::dummy(trait_ref).to_host_effect_clause(interner, BoundConstness::Maybe);
            let obligation = Obligation::new(
                interner,
                ObligationCause::dummy(),
                ParamEnv::empty(),
                clause.as_predicate(),
            );
            (non_const_impl(&infcx, &obligation), impl_)
        };
        let (plain, plain_impl) = named("Plain");
        assert_eq!(plain, Some(AnyImplId::ImplId(plain_impl)), "`impl Plain for u8` is not const");
        let (const_, _) = named("Const");
        assert_eq!(const_, None, "`impl const Const for u8` is const");
    });
}

//! rustc's `explicit_implied_const_bounds` of an associated type and of an opaque: the
//! `[const] Trait` written on each, as `HostEffect(Maybe)`, kept out of its item bounds.

use base_db::Db;
use hir_def::{
    FunctionId, ModuleDefId, TraitId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TraitSignature},
};
use rustc_type_ir::{BoundConstness, ClauseKind, inherent::IntoKind as _};
use hir_expand::fixture;

use crate::{
    ImplTraitId,
    db::InternedOpaqueTyId,
    lower::{ImplTraitIdx, implied_const_bounds, type_alias_bounds},
    next_solver::{Clause, util::ItemWithBounds},
};

const FIXTURE: &str = r#"
//- minicore: sized
#![feature(const_trait_impl)]
const trait Tr {}
trait Plain {}
const trait Holder {
    type Out: [const] Tr + Plain;
}
impl const Tr for () {}
impl Plain for () {}
const fn make() -> impl [const] Tr + Plain {}
"#;

fn declarations(db: &Db) -> Vec<ModuleDefId> {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

fn trait_named(db: &Db, name: &str) -> TraitId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::TraitId(it) if TraitSignature::of(db, it).name.as_str() == name => Some(it),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no trait `{name}` in the fixture"))
}

fn make(db: &Db) -> FunctionId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == "make" => {
                Some(it)
            }
            _ => None,
        })
        .expect("the fixture has `make`")
}

/// The trait each clause is a `[const]` bound of; panics on a clause that is not one.
fn maybe_const_traits(clauses: &[Clause<'_>]) -> Vec<TraitId> {
    clauses
        .iter()
        .map(|clause| match clause.kind().skip_binder() {
            ClauseKind::HostEffect(host) => {
                assert_eq!(host.constness, BoundConstness::Maybe);
                host.trait_ref.def_id.0
            }
            other => panic!("not a `[const]` bound: {other:?}"),
        })
        .collect()
}

fn has_host_effect(clauses: &[Clause<'_>]) -> bool {
    clauses.iter().any(|clause| matches!(clause.kind().skip_binder(), ClauseKind::HostEffect(_)))
}

#[test]
fn an_associated_type_and_an_opaque_imply_their_const_bounds() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let tr = trait_named(&db, "Tr");
        let out = trait_named(&db, "Holder")
            .trait_items(&db)
            .associated_types()
            .next()
            .expect("`Holder` has `Out`");
        let alias = implied_const_bounds(&db, ItemWithBounds::TypeAliasId(out)).skip_binder();
        assert_eq!(maybe_const_traits(alias), [tr], "`Out: [const] Tr + Plain` implies `Tr` only");
        assert!(
            !has_host_effect(type_alias_bounds(&db, out).skip_binder()),
            "the item bounds stay without host effects"
        );

        let opaque = InternedOpaqueTyId::new(
            &db,
            ImplTraitId::ReturnTypeImplTrait(
                make(&db),
                ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(0)),
            ),
        );
        let rpit = implied_const_bounds(&db, ItemWithBounds::InternedOpaqueTyId(opaque));
        assert_eq!(maybe_const_traits(rpit.skip_binder()), [tr]);
        assert!(!has_host_effect(opaque.loc(&db).bounds(&db).all_bounds().skip_binder()));
    });
}

//! rustc's `explicit_implied_const_bounds` of an associated type and of an opaque: the
//! `[const] Trait` written on each, as `HostEffect(Maybe)`, kept out of its item bounds.

use base_db::Db;
use hir_def::{
    FunctionId, ModuleDefId, TraitId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TraitSignature},
};
use hir_expand::fixture;
use rustc_type_ir::{BoundConstness, ClauseKind, inherent::IntoKind as _};

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
            ModuleDefId::TraitId(it) if TraitSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no trait `{name}` in the fixture"))
}

fn make(db: &Db) -> FunctionId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it)
                if FunctionSignature::of(db, it).name.as_str() == "make" =>
            {
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
    blaze_query::db::attach(&db, || {
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

const RPITIT_FIXTURE: &str = r#"
//- minicore: sized
#![feature(const_trait_impl)]
const trait Bar {}
const trait Foo {
    fn method(self) -> impl [const] Bar;
}
"#;

/// rustc's `is_conditionally_const` and `explicit_implied_const_bounds` of a const trait's
/// RPITIT (`const-cond-for-rpitit.rs`): conditionally const, as an associated type of the
/// trait is, with the `[const] Bar` written on its `impl Trait`, so under `T: [const] Foo` the
/// value of `T::method` is `[const] Bar`.
#[test]
fn a_const_traits_impl_trait_implies_its_const_bounds() {
    use rustc_type_ir::Interner as _;
    let db = fixture::db(RPITIT_FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let bar = trait_named(&db, "Bar");
        let method = trait_named(&db, "Foo")
            .trait_items(&db)
            .items
            .iter()
            .find_map(|(_, item)| match *item {
                hir_def::AssocItemId::FunctionId(it) => Some(it),
                _ => None,
            })
            .expect("`Foo` has `method`");
        let interner = crate::next_solver::DbInterner::new_no_crate(&db);
        let item = crate::next_solver::SolverDefId::RpititId(crate::rpitit::RpititId {
            function: method,
            idx: ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(0)),
        });
        assert!(interner.alias_has_const_conditions(item), "the item of a const trait is");
        let implied: Vec<TraitId> = interner // alloc: test
            .explicit_implied_const_bounds(item)
            .skip_binder()
            .into_iter()
            .map(|it| it.skip_binder().def_id.0)
            .collect();
        assert_eq!(implied, [bar], "`impl [const] Bar` implies `[const] Bar`");
    });
}

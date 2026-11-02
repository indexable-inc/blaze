//! rustc's `add_implicit_sizedness_bounds`: a type parameter or associated type with no
//! sizedness bound is `Sized`, one bound `?Sized` alone is `MetaSized`, an explicit `MetaSized` is
//! kept and a `PointeeSized` adds nothing; a trait's `Self` is `MetaSized` unless it has a
//! sizedness bound or the trait is an auto trait.

use base_db::Db;
use hir_def::{
    GenericDefId, ModuleDefId, TraitId,
    lang_item::lang_items,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TraitSignature, TypeAliasSignature},
};
use rustc_type_ir::inherent::{Clause as _, IntoKind as _};
use hir_expand::fixture;

use crate::{
    GenericPredicates,
    lower::type_alias_bounds,
    next_solver::{Clause, TyKind},
};

const FIXTURE: &str = r#"
//- minicore: sized
#![feature(sized_hierarchy, auto_traits)]
use core::marker::{MetaSized, PointeeSized};
fn f<A, B: ?Sized, C: MetaSized, D: PointeeSized, E: ?Sized + Sized>() {}
trait Plain {}
trait Pointee: PointeeSized {}
trait Bare: Sized {}
auto trait Auto {}
trait Holder {
    type Unsized: ?Sized;
    type Plain;
}
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

/// Each sizedness clause of `clauses`: the index of its self parameter and its trait's name.
fn sizedness<'db>(
    db: &Db,
    clauses: impl IntoIterator<Item = Clause<'db>>,
) -> Vec<(u32, &'static str)> {
    let lang = lang_items(db, fixture::test_crate(db));
    let names =
        [(lang.Sized, "Sized"), (lang.MetaSized, "MetaSized"), (lang.PointeeSized, "PointeeSized")];
    clauses
        .into_iter()
        .filter_map(|clause| {
            let trait_ref = clause.as_trait_clause()?.skip_binder().trait_ref;
            let (_, name) = names.iter().find(|(it, _)| *it == Some(trait_ref.def_id.0))?;
            match trait_ref.self_ty().kind() {
                TyKind::Param(param) => Some((param.index, *name)),
                other => panic!("a sizedness clause on {other:?}"),
            }
        })
        .collect()
}

#[test]
fn implicit_sizedness_bounds_are_rustcs() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let f = declarations(&db)
            .into_iter()
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            })
            .filter(|&it| FunctionSignature::of(&db, it).name.as_str() == "f")
            .expect("the fixture's one function is `f`");
        let predicates = GenericPredicates::query_all(&db, GenericDefId::FunctionId(f));
        let mut of_f = sizedness(&db, predicates.skip_binder());
        of_f.sort();
        assert_eq!(
            of_f,
            [(0, "Sized"), (1, "MetaSized"), (2, "MetaSized"), (4, "Sized")],
            "A: Sized, B: ?Sized is MetaSized, C keeps its MetaSized only, D none, E its Sized only"
        );

        let of_trait = |name| {
            sizedness(
                &db,
                GenericPredicates::query_all(&db, GenericDefId::TraitId(trait_named(&db, name)))
                    .skip_binder(),
            )
        };
        assert_eq!(of_trait("Plain"), [(0, "MetaSized")]);
        assert_eq!(of_trait("Pointee"), [], "a `PointeeSized` supertrait relaxes `Self`");
        assert_eq!(of_trait("Bare"), [(0, "Sized")], "a `Sized` supertrait, and no `MetaSized`");
        assert_eq!(of_trait("Auto"), [], "an auto trait gets no supertrait");

        let alias = |name| {
            let alias = trait_named(&db, "Holder")
                .trait_items(&db)
                .associated_types()
                .find(|&it| TypeAliasSignature::of(&db, it).name.as_str() == name)
                .unwrap_or_else(|| panic!("`Holder` has `{name}`"));
            let bounds = type_alias_bounds(&db, alias).skip_binder();
            let lang = lang_items(&db, fixture::test_crate(&db));
            bounds
                .iter()
                .filter_map(|clause| clause.as_trait_clause())
                .map(|it| it.skip_binder().trait_ref.def_id.0)
                .filter(|&it| Some(it) == lang.Sized || Some(it) == lang.MetaSized)
                .collect::<Vec<_>>()
        };
        let lang = lang_items(&db, fixture::test_crate(&db));
        assert_eq!(alias("Unsized"), [lang.MetaSized.unwrap()]);
        assert_eq!(alias("Plain"), [lang.Sized.unwrap()]);
    });
}

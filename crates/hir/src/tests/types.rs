//! The type questions blaze's clippy lints ask (`needless_pass_by_value`,
//! `multiple_unsafe_ops_per_block`, `box_collection`, `vec_box`): a type's own bounds in its
//! environment, a reference to it, whether it is sized, may implement `Copy` or implements a
//! trait for some arguments, and a callable's safety.

use base_db::Db;
use hir_expand::fixture;

use super::db::TestDB;
use crate::{
    Crate, DisplayTarget, Function, HirDisplay, ModuleDef, Mutability, Safety, Trait, Type,
};

/// The item named `name` at the root of a crate of the fixture.
fn item(db: &TestDB, name: &str) -> ModuleDef {
    Crate::all(db)
        .into_iter()
        .flat_map(|krate| krate.root_module(db).declarations_iter(db))
        .find(|it| it.name(db).is_some_and(|it| it.as_str() == name))
        .unwrap_or_else(|| panic!("no item `{name}` in the fixture"))
}

fn func(db: &TestDB, name: &str) -> Function {
    match item(db, name) {
        ModuleDef::Function(it) => it,
        it => panic!("`{name}` is {it:?}"),
    }
}

fn trait_(db: &TestDB, name: &str) -> Trait {
    match item(db, name) {
        ModuleDef::Trait(it) => it,
        it => panic!("`{name}` is {it:?}"),
    }
}

/// The types of `name`'s parameters, in order.
fn params<'db>(db: &'db TestDB, name: &str) -> Vec<Type<'db>> {
    func(db, name).signature(db).expect("a function has a signature").params(db)
}

fn target(db: &TestDB, name: &str) -> DisplayTarget {
    func(db, name).module(db).krate(db).to_display_target(db)
}

fn show<'db>(db: &'db Db, ty: &Type<'db>, target: DisplayTarget) -> String {
    ty.display(db, target).to_string()
}

const TYPES: &str = r#"
//- minicore: sized, copy, drop
//- /main.rs crate:main
#![feature(unsafe_fields)]
trait Show {}
trait Sub: Show {}
trait Pair<T> {}
trait Lt<'a> {}
trait Any<T> {}
impl<T: Show> Show for &T {}
struct Plain(u8, bool);
struct Holds(Plain);
struct Generic<T>(T);
enum Either<T> { A(T), B(u8) }
struct Dropped(u8);
impl Drop for Dropped { fn drop(&mut self) {} }
struct Guarded { unsafe raw: u8 }
impl Any<u8> for Plain {}
fn bounds<T: Sub + Pair<u8>, U: Show>(t: T, u: U) where for<'a> T: Lt<'a> {}
fn adts<C: Copy, D>(p: Plain, h: Holds, d: Dropped, g: Guarded, gc: Generic<C>, gd: Generic<D>, ec: Either<C>, ed: Either<D>) {}
fn others<T: ?Sized>(t: &T, s: &[u8], m: &mut u8) {}
fn callees(f: unsafe fn(), g: fn(), h: unsafe extern "C" fn(u8) -> u8) {}
unsafe fn danger() {}
fn safe() {}
"#;

#[test]
fn a_type_s_bounds_are_its_own_elaborated_without_sized() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let [t, u] = <[Type<'_>; 2]>::try_from(params(&db, "bounds")).unwrap();
        let named = |ty: &Type<'_>| {
            let mut names: Vec<(String, Vec<String>)> = ty
                .env_bounds(&db)
                .into_iter()
                .map(|(trait_, args)| {
                    let args = args.iter().map(|it| show(&db, it, target(&db, "bounds")));
                    (trait_.name(&db).as_str().to_owned(), args.collect())
                })
                .collect();
            names.sort();
            names
        };
        let none = Vec::<String>::new;
        assert_eq!(
            named(&t),
            [
                ("Pair".to_owned(), vec!["u8".to_owned()]),
                ("Show".to_owned(), none()),
                ("Sub".to_owned(), none()),
            ],
            "`Show` by elaboration of `Sub`; not `Sized`, nor `for<'a> T: Lt<'a>`, which binds `'a`",
        );
        assert_eq!(named(&u), [("Show".to_owned(), none())], "only `U`'s, not `T`'s");
        let reference = t.ref_to(&db, Mutability::Shared);
        assert!(named(&reference).is_empty(), "no clause has `&T` for its self type");
    });
}

#[test]
fn a_reference_to_a_type_has_its_environment() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let [t, u] = <[Type<'_>; 2]>::try_from(params(&db, "bounds")).unwrap();
        let shared = t.ref_to(&db, Mutability::Shared);
        assert!(shared.is_reference() && !shared.is_mutable_reference());
        assert_eq!(shared.as_reference_inner(), Some(t.clone()));
        assert_eq!(show(&db, &shared, target(&db, "bounds")), "&T");
        assert!(t.ref_to(&db, Mutability::Mut).is_mutable_reference());

        let show_trait = trait_(&db, "Show");
        let pair = trait_(&db, "Pair");
        let u8_arg = params(&db, "adts")[0].fields(&db).remove(0).1;
        assert!(shared.impls_trait(&db, show_trait, &[]), "`impl<T: Show> Show for &T` and `T: Sub`");
        assert!(u.ref_to(&db, Mutability::Shared).impls_trait(&db, show_trait, &[]));
        let u8_arg = u8_arg.without_params(&db).expect("`u8` names no parameter");
        assert!(t.impls_trait(&db, pair, &[u8_arg.clone()]));
        assert!(!shared.impls_trait(&db, pair, &[u8_arg]), "no `Pair` impl for references");
    });
}

#[test]
fn a_reference_with_an_error_region_is_solved_modulo_regions() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let [t, _] = <[Type<'_>; 2]>::try_from(params(&db, "bounds")).unwrap();
        let errored = t.add_reference(&db, Mutability::Shared);
        let u8_arg = params(&db, "adts")[0].fields(&db).remove(0).1;
        let u8_arg = u8_arg.without_params(&db).expect("`u8` names no parameter");
        assert!(errored.impls_trait(&db, trait_(&db, "Show"), &[]), "`impl<T: Show> Show for &T`");
        assert!(
            !errored.impls_trait(&db, trait_(&db, "Pair"), &[u8_arg]),
            "an error region is no error goal the solver proves: no `Pair` impl for references",
        );
    });
}

#[test]
fn sized_is_the_solver_s_answer_in_the_environment() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let [t, s, m] = <[Type<'_>; 3]>::try_from(params(&db, "others")).unwrap();
        assert!(t.is_sized(&db), "`&T`");
        assert!(!t.as_reference_inner().unwrap().is_sized(&db), "`T: ?Sized`");
        assert!(!s.as_reference_inner().unwrap().is_sized(&db), "`[u8]`");
        assert!(m.as_reference_inner().unwrap().is_sized(&db), "`u8`");
        assert!(params(&db, "bounds")[0].is_sized(&db), "`T`, implicitly `Sized`");
        assert!(params(&db, "adts")[0].is_sized(&db), "`Plain`");
    });
}

#[test]
fn copy_is_allowed_of_an_adt_without_drop_unsafe_fields_or_a_field_not_copy() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let adts = params(&db, "adts");
        let allowed: Vec<bool> = adts.iter().map(|it| it.may_implement_copy(&db)).collect();
        assert_eq!(
            allowed,
            [true, false, false, false, true, false, true, false],
            "`Plain` yes; `Holds` holds a `Plain`, not `Copy`; `Dropped` has a `Drop` impl; \
             `Guarded` an unsafe field; `Generic<C>` and `Either<C>` with `C: Copy` yes, \
             with `D` no",
        );
        assert!(adts.iter().all(|it| !it.is_copy(&db)), "none implements `Copy` already");

        let [t, s, m] = <[Type<'_>; 3]>::try_from(params(&db, "others")).unwrap();
        assert!(t.may_implement_copy(&db), "`&T` has core's impl");
        assert!(!m.may_implement_copy(&db), "`&mut u8` is no ADT");
        assert!(!s.as_reference_inner().unwrap().may_implement_copy(&db), "`[u8]` is no ADT");
    });
}

#[test]
fn a_trait_with_fresh_arguments_holds_for_some_impl() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let any = trait_(&db, "Any");
        let adts = params(&db, "adts");
        let (plain, holds) = (&adts[0], &adts[1]);
        assert!(plain.impls_trait_fresh(&db, any), "`impl Any<u8> for Plain`");
        assert!(!plain.impls_trait(&db, any, &[]), "an error argument never holds");
        assert!(!holds.impls_trait_fresh(&db, any));

        let [t, _] = <[Type<'_>; 2]>::try_from(params(&db, "bounds")).unwrap();
        assert!(t.impls_trait_fresh(&db, trait_(&db, "Pair")), "`T: Pair<u8>` in the environment");
    });
}

#[test]
fn a_callable_s_safety_is_its_signature_s() {
    let db = fixture::db(TYPES).into_db();
    crate::attach_db(&db, || {
        let safety = |ty: &Type<'_>| ty.as_callable(&db).expect("a callable type").safety();
        let [f, g, h] = <[Type<'_>; 3]>::try_from(params(&db, "callees")).unwrap();
        assert_eq!(safety(&f), Safety::Unsafe, "`unsafe fn()`");
        assert_eq!(safety(&g), Safety::Safe, "`fn()`");
        assert_eq!(safety(&h), Safety::Unsafe, "`unsafe extern \"C\" fn(u8) -> u8`");
        assert_eq!(safety(&func(&db, "danger").ty(&db)), Safety::Unsafe, "`unsafe fn danger`");
        assert_eq!(safety(&func(&db, "safe").ty(&db)), Safety::Safe);
    });
}

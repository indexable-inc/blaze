//! `impl_rpitit_tys`, rustc's `collect_return_position_impl_trait_in_trait_tys`: the value of
//! each impl's item for a trait method's `impl Trait`, in the impl item's generics, with the
//! regions it captures mapped to the trait item's own lifetime parameters.

use hir_def::{
    AssocItemId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{
    display::{DisplayTarget, HirDisplay},
    rpitit::{RpititError, impl_rpitit_tys},
};

/// The values the impl method `name`'s items have, each displayed, or the error.
fn values(fixture: &str, name: &str) -> Vec<Result<String, RpititError>> {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let krate = fixture::test_crate(&db);
        let mut found: Option<FunctionId> = None;
        for (_, module) in crate_def_map(&db, krate).modules(&db) {
            for impl_ in module.scope(&db).impls() {
                for (_, item) in impl_.impl_items(&db).items.iter() {
                    if let AssocItemId::FunctionId(it) = *item
                        && FunctionSignature::of(&db, it).name.as_str() == name
                    {
                        found = Some(it);
                    }
                }
            }
        }
        let impl_fn = found.unwrap_or_else(|| panic!("no impl method `{name}`"));
        let target = DisplayTarget::from_crate(&db, krate);
        impl_rpitit_tys(&db, impl_fn)
            .iter()
            .map(|value| match value {
                Ok(ty) => Ok(ty.get().skip_binder().display_test(&db, target).to_string()),
                Err(error) => Err(error.clone()),
            })
            .collect() // alloc: test
    })
}

#[test]
fn an_impls_value_is_what_its_method_returns_there() {
    let found = values(
        "//- minicore: sized\n\
         pub struct Vec<T>(T);\n\
         pub trait Tr { fn f(&self) -> impl Sized; fn g(self) -> (u8, impl Sized) where Self: Sized; }\n\
         pub struct S;\n\
         impl Tr for S { fn f(&self) -> Vec<u32> { Vec(0) } fn g(self) -> (u8, u16) { (0, 0) } }\n",
        "f",
    );
    assert_eq!(found, [Ok("Vec<u32>".to_owned())]); // alloc: test
    let found = values(
        "//- minicore: sized\n\
         pub trait Tr { fn g(self) -> (u8, impl Sized) where Self: Sized; }\n\
         pub struct S;\n\
         impl Tr for S { fn g(self) -> (u8, u16) { (0, 0) } }\n",
        "g",
    );
    assert_eq!(found, [Ok("u16".to_owned())]); // alloc: test
}

/// The impl's own `impl Trait` is its value, an opaque type of the impl's method.
#[test]
fn an_impls_own_impl_trait_is_its_value() {
    let found = values(
        "//- minicore: sized, copy\n\
         pub trait Tr { fn f(&self) -> impl Sized; }\n\
         pub struct S;\n\
         impl Tr for S { fn f(&self) -> impl Copy { 0u8 } }\n",
        "f",
    );
    assert_eq!(found, [Ok("impl Copy".to_owned())]); // alloc: test
}

/// `elided &self` and an explicit lifetime: the impl's region is the trait's, which the trait's
/// item has as its own parameter.
#[test]
fn a_captured_lifetime_is_the_trait_items_own() {
    let found = values(
        "//- minicore: sized\n\
         pub struct Iter<'a, T>(&'a T);\n\
         pub trait Tr { fn f(&self) -> impl Sized; fn g<'a>(&'a self, x: &'a u8) -> impl Sized + 'a; }\n\
         pub struct S(u32);\n\
         impl Tr for S {\n\
             fn f(&self) -> Iter<'_, u32> { Iter(&self.0) }\n\
             fn g<'b>(&'b self, x: &'b u8) -> Iter<'b, u8> { Iter(x) }\n\
         }\n",
        "g",
    );
    // The impl's `'b` is the trait method's `'a`, the trait item's own parameter.
    assert_eq!(found, [Ok("Iter<'a, u8>".to_owned())]); // alloc: test
    let found = values(
        "//- minicore: sized\n\
         pub struct Iter<'a, T>(&'a T);\n\
         pub trait Tr { fn f(&self) -> impl Sized; }\n\
         pub struct S(u32);\n\
         impl Tr for S { fn f(&self) -> Iter<'_, u32> { Iter(&self.0) } }\n",
        "f",
    );
    // The elided lifetime of the trait's `&self`, which its item captures.
    assert_eq!(found, [Ok("Iter<'_, u32>".to_owned())]); // alloc: test
}

/// A lifetime of the impl's method at a place where the trait's has one of its parameters' is
/// that one; `'static` stays `'static`.
#[test]
fn a_parameters_lifetime_maps_and_static_stays() {
    let found = values(
        "//- minicore: sized\n\
         pub struct Iter<'a, T>(&'a T);\n\
         pub trait Tr { fn f(&self, x: &u8) -> impl Sized; }\n\
         pub struct S(u32);\n\
         impl Tr for S { fn f<'a>(&self, x: &'a u8) -> Iter<'a, u8> { Iter(x) } }\n",
        "f",
    );
    // The trait's `&u8` lifetime is in scope of its `impl Trait` (every lifetime in scope is
    // captured), so `'a` maps to it.
    assert_eq!(found, [Ok("Iter<'_, u8>".to_owned())]); // alloc: test
    let found = values(
        "//- minicore: sized\n\
         pub struct Iter<'a, T>(&'a T);\n\
         pub trait Tr { fn f(&self) -> impl Sized; }\n\
         pub struct S(u32);\n\
         static X: u8 = 0;\n\
         impl Tr for S { fn f(&self) -> Iter<'static, u8> { Iter(&X) } }\n",
        "f",
    );
    assert_eq!(found, [Ok("Iter<'static, u8>".to_owned())]); // alloc: test
}

/// `nested-rpitit.rs`: an `impl Trait` in the bounds of another has the type those bounds give
/// once the outer one's is known.
#[test]
fn a_nested_impl_traits_value_comes_from_the_outer_ones_bounds() {
    let found = values(
        "//- minicore: sized, iterator\n\
         pub struct Once<T>(T);\n\
         impl<T> Iterator for Once<T> { type Item = T; fn next(&mut self) -> Option<T> { loop {} } }\n\
         pub trait Tr { fn items(&self) -> impl Iterator<Item = impl Sized>; }\n\
         pub struct S;\n\
         impl Tr for S { fn items(&self) -> Once<u16> { Once(0) } }\n",
        "items",
    );
    assert_eq!(found, [Ok("Once<u16>".to_owned()), Ok("u16".to_owned())]); // alloc: test
}

/// rustc's hybrid environment: the bounds of an `impl Trait` nested in another's are proven
/// with the method's own where clauses too, so the inner `impl Clone` is `T` by `T: Clone`.
#[test]
fn a_nested_impl_traits_bounds_are_proven_with_the_methods_where_clauses() {
    let found = values(
        "//- minicore: sized, iterator, clone\n\
         pub struct Once<T>(T);\n\
         impl<T> Iterator for Once<T> { type Item = T; fn next(&mut self) -> Option<T> { loop {} } }\n\
         pub trait Tr { fn items<T: Clone>(t: T) -> impl Iterator<Item = impl Clone>; }\n\
         pub struct S;\n\
         impl Tr for S { fn items<T: Clone>(t: T) -> Once<T> { Once(t) } }\n",
        "items",
    );
    assert_eq!(found, [Ok("Once<T>".to_owned()), Ok("T".to_owned())]); // alloc: test
}

/// rustc normalizes both signatures before relating them: an impl that writes its return type
/// through an associated type of its own gives the type it normalizes to.
#[test]
fn an_impl_may_write_its_return_type_through_an_associated_type() {
    let found = values(
        "//- minicore: sized\n\
         pub struct W<T>(T);\n\
         pub trait Tr { type Out; fn f(&self) -> W<impl Sized>; }\n\
         pub struct S;\n\
         impl Tr for S { type Out = W<u8>; fn f(&self) -> Self::Out { W(0) } }\n",
        "f",
    );
    assert_eq!(found, [Ok("u8".to_owned())]); // alloc: test
    // The item's type is the normalized one, as rustc's `type_of` of it, not the projection.
    let found = values(
        "//- minicore: sized\n\
         pub struct W<T>(T);\n\
         pub trait Tr { type Out; fn f(&self) -> impl Sized; }\n\
         pub struct S;\n\
         impl Tr for S { type Out = W<u8>; fn f(&self) -> Self::Out { W(0) } }\n",
        "f",
    );
    assert_eq!(found, [Ok("W<u8>".to_owned())]); // alloc: test
}

/// rustc fails the comparison where a signature names an associated type that does not
/// normalize (`S` implements no `Q`): the impl's type for the item is not that projection.
#[test]
fn a_return_type_that_does_not_normalize_gives_no_type() {
    let found = values(
        "//- minicore: sized\n\
         pub trait Q { type X; }\n\
         pub trait Tr { fn f(&self) -> impl Sized; }\n\
         pub struct S;\n\
         impl Tr for S { fn f(&self) -> <S as Q>::X { loop {} } }\n",
        "f",
    );
    assert_eq!(found, [Err(RpititError::Unnormalized)]); // alloc: test
}

/// A free type alias expands where it is lowered.
#[test]
fn an_impl_may_write_its_return_type_through_a_type_alias() {
    let found = values(
        "//- minicore: sized\n\
         pub struct W<T>(T);\n\
         pub type O = W<u8>;\n\
         pub trait Tr { fn f(&self) -> W<impl Sized>; }\n\
         pub struct S;\n\
         impl Tr for S { fn f(&self) -> O { W(0) } }\n",
        "f",
    );
    assert_eq!(found, [Ok("u8".to_owned())]); // alloc: test
}

/// An early-bound lifetime of the method (one a where clause names) is the impl item's own.
#[test]
fn an_early_bound_lifetime_of_the_method_is_the_impl_items_own() {
    let found = values(
        "//- minicore: sized\n\
         pub struct Iter<'a, T>(&'a T);\n\
         pub trait Tr { fn g<'a>(&self, x: &'a u8) -> impl Sized + 'a where 'a: 'a; }\n\
         pub struct S;\n\
         impl Tr for S { fn g<'a>(&self, x: &'a u8) -> Iter<'a, u8> where 'a: 'a { Iter(x) } }\n",
        "g",
    );
    assert_eq!(found, [Ok("Iter<'a, u8>".to_owned())]); // alloc: test
}

/// An impl lifetime the trait's signature has at two places of two lifetimes is none of the
/// trait item's: the error names it as written.
#[test]
fn an_uncaptured_lifetime_is_named_as_written() {
    let found = values(
        "//- minicore: sized\n\
         pub struct Iter<'a, T>(&'a T);\n\
         pub trait Tr { fn g<'b, 'c>(&self, x: &'b u8, y: &'c u8) -> impl Sized; }\n\
         pub struct S;\n\
         impl Tr for S { fn g<'a>(&self, x: &'a u8, y: &'a u8) -> Iter<'a, u8> { Iter(x) } }\n",
        "g",
    );
    assert!(
        matches!(&found[..], [Err(RpititError::UncapturedLifetime { lifetime, .. })] if &**lifetime == "'a"),
        "{found:?}"
    );
}

/// `doesnt-satisfy.rs`: the impl's type must meet the bounds of the trait's `impl Trait`
/// (rustc's E0277 "required by a bound in `Foo::bar::{anon_assoc#0}`"); each bound that fails
/// is named with the item it bounds, and `Copy`'s supertrait `Clone`, which it implies, is not.
#[test]
fn a_value_that_fails_the_traits_bounds_is_unsatisfied() {
    let found = values(
        "//- minicore: sized, copy\n\
         pub struct NotCopy;\n\
         pub trait Tr { fn f(&self) -> (impl Sized, impl Copy); }\n\
         pub struct S;\n\
         impl Tr for S { fn f(&self) -> (u8, NotCopy) { (0, NotCopy) } }\n",
        "f",
    );
    assert!(
        matches!(&found[..], [Err(RpititError::Unsatisfied { of, .. }), Err(RpititError::Unsatisfied { .. })] if **of == [1]),
        "{found:?}"
    );
}

/// rustc's `predicates_of` an impl's RPITIT item is the trait item's rebased onto the impl
/// (`gather_explicit_predicates_of`, `ImplTraitInTraitData::Impl`): the trait method's `T: Copy`,
/// which the impl's method leaves out, is a where clause of the impl's item, as normalizing the
/// trait's item to the impl's reads it (`own_clauses_of`).
#[test]
fn the_impls_item_has_the_trait_methods_where_clauses() {
    use rustc_type_ir::{ClauseKind, Interner as _, inherent::IntoKind as _};
    let db = fixture::db(
        "//- minicore: sized, copy\n\
         pub trait Tr { fn f<T: Copy>(&self, t: T) -> impl Copy; }\n\
         pub struct S;\n\
         impl Tr for S { fn f<T>(&self, t: T) -> T { t } }\n",
    )
    .into_db();
    blaze_query::db::attach(&db, || {
        let krate = fixture::test_crate(&db);
        let impl_fn = crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).impls().collect::<Vec<_>>()) // alloc: test
            .flat_map(|impl_| impl_.impl_items(&db).items.to_vec()) // alloc: test
            .find_map(|(_, item)| match item {
                AssocItemId::FunctionId(it) => Some(it),
                _ => None,
            })
            .expect("the impl has `f`");
        let interner = crate::next_solver::DbInterner::new_no_crate(&db);
        let item = crate::rpitit::RpititId {
            function: impl_fn,
            idx: crate::next_solver::ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(0)),
        };
        let traits: Vec<String> =
            interner // alloc: test
                .own_clauses_of(crate::next_solver::SolverDefId::RpititId(item))
                .skip_binder()
                .into_iter()
                .filter_map(|clause| match clause.kind().skip_binder() {
                    ClauseKind::Trait(it) => Some(
                        hir_def::signatures::TraitSignature::of(&db, it.trait_ref.def_id.0)
                            .name
                            .as_str()
                            .to_owned(),
                    ),
                    _ => None,
                })
                .collect();
        assert!(traits.iter().any(|it| it == "Copy"), "{traits:?}");
    });
}

//! `impl_rpitit_tys`, rustc's `collect_return_position_impl_trait_in_trait_tys`: the value of
//! each impl's item for a trait method's `impl Trait`, in the impl item's generics, with the
//! regions it captures mapped to the trait item's own lifetime parameters.

use hir_def::{AssocItemId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
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

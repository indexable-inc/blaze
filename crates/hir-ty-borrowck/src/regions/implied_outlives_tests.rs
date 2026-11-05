//! What a body may assume of its free regions from its signature, as rustc's
//! `UniversalRegionRelations` collects it: a where clause `Self: 'b` on a type with lifetimes
//! split into its outlives components (`hegeltest`'s `BoxedGenerator::boxed`, `Self:
//! 'b` with `Self = BoxedGenerator<'a, T>` that `'a: 'b`), and a reference to an associated
//! type in the signature that the type outlives it, tested as the callee's projection
//! normalizes (`sonic-rs`'s `Index for Array`, `&Self::Output` returning
//! `Index::index(&**self, index)`).

use base_db::{Db, all_crates};
use hir_def::{
    AssocItemId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized
trait Gen<T> {}

struct Boxed<'a, T> {
    inner: &'a (dyn Gen<T> + 'a),
}

impl<'a, T> Boxed<'a, T> {
    fn shorten<'b>(self) -> Boxed<'b, T>
    where
        Self: 'b,
    {
        Boxed { inner: self.inner }
    }

    fn shorten_unbounded<'b>(self) -> Boxed<'b, T> {
        Boxed { inner: self.inner }
    }
}

trait Idx<T: ?Sized> {
    type Output: ?Sized;
}

trait Get<I> {
    type Output: ?Sized;
    fn get(&self, i: I) -> &Self::Output;
}

impl<T, I: Idx<[T]>> Get<I> for [T] {
    type Output = I::Output;
    fn get(&self, _i: I) -> &I::Output {
        loop {}
    }
}

trait Look<I> {
    type Output: ?Sized;
    fn look(&self, i: I) -> &Self::Output;
}

struct A {
    s: &'static [u8],
}

impl<I: Idx<[u8]>> Look<I> for A {
    type Output = I::Output;
    fn look(&self, i: I) -> &Self::Output {
        Get::get(self.s, i)
    }
}

fn unimplied<'r, I: Idx<[u8]>>(s: &'r [u8], i: I) -> &'r u8 {
    let _x: &'r <[u8] as Get<I>>::Output = Get::get(s, i);
    &0
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            let free = scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            });
            let methods = scope.impls().flat_map(|it| {
                it.impl_items(db).items.iter().filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                })
            });
            for it in free.chain(methods) {
                if FunctionSignature::of(db, it).name.as_str() == name {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}`")
}

fn checked(name: &str) -> [BorrowckResult; 2] {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius].map(|mode| borrowck_in_mode(&db, function.into(), mode))
    })
}

fn accepted(name: &str) {
    for result in checked(name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert_eq!(result.skipped, None, "`{name}`");
        assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
    }
}

fn rejected(name: &str, message: &str) {
    for result in checked(name) {
        assert_eq!(result.not_checked, None, "`{name}`");
        assert!(
            result.diagnostics.iter().any(|it| it.message == message),
            "`{name}`: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn a_where_clause_self_outlives_implies_its_lifetimes_outlive() {
    accepted("shorten");
}

#[test]
fn without_the_where_clause_the_lifetimes_are_unrelated() {
    rejected("shorten_unbounded", "lifetime may not live long enough");
}

#[test]
fn a_reference_to_a_projection_in_the_signature_implies_it_outlives() {
    accepted("look");
}

#[test]
fn a_projection_the_signature_does_not_name_is_not_implied_to_outlive() {
    rejected("unimplied", "the associated type may not live long enough");
}

//! A provided item whose own where-clauses may hold through an impl is not impossible there, as
//! rustc's `is_impossible_associated_item` answers rustdoc (queue 01225): its clauses are solved
//! in coherence mode with the impl's parameters as inference variables, and only a clause that
//! fails for every value of them makes the item impossible.
//!
//! Each method of `Each` asks one variant of the clause, so a failure names the solver step.

use hir_def::{AdtId, AssocItemId, GenericDefId, ImplId, nameres::crate_def_map, signatures::StructSignature};
use hir_expand::fixture;

use crate::db::HirDatabase;

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone
pub struct Square;
pub trait Mark {}
impl<T: ?Sized> Mark for &T {}
pub trait Named {}
impl<'b, T: ?Sized> Named for &'b T {}
pub trait Each {
    type Item;
    fn copied(self) where Self::Item: Copy, Self: Sized {}
    fn borrowed(&self) where for<'a> &'a Self::Item: Copy {}
    fn borrowed_static(&self) where &'static Self::Item: Copy {}
    fn borrowed_sized(&self) where for<'a> &'a Self::Item: Sized {}
    fn borrowed_local_trait(&self) where for<'a> &'a Self::Item: Mark {}
    fn borrowed_named_impl_lifetime(&self) where for<'a> &'a Self::Item: Named {}
}
pub struct Squares;
impl Each for Squares { type Item = Square; }
pub struct Finds<A>(A);
impl<A: Clone> Each for Finds<A> { type Item = Square; }
pub struct Borrows<'a>(&'a u8);
impl<'a> Each for Borrows<'a> { type Item = Square; }
"#;

/// Whether `method` of `Each` is impossible through the impl for the struct `self_ty`.
fn impossible(method: &str, self_ty: &str) -> bool {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let krate = fixture::test_crate(&db);
        let def_map = crate_def_map(&db, krate);
        let scope = def_map.modules(&db).next().expect("the root module").1.scope(&db);
        let impl_: ImplId = scope
            .impls()
            .find(|&it| match db.impl_self_ty(it).skip_binder().as_adt() {
                Some((AdtId::StructId(s), _)) => StructSignature::of(&db, s).name.as_str() == self_ty,
                _ => false,
            })
            .unwrap_or_else(|| panic!("the fixture's `impl Each for {self_ty}`"));
        let trait_ = db.impl_trait(impl_).expect("a trait impl").skip_binder().def_id.0;
        let item = trait_
            .trait_items(&db)
            .items
            .iter()
            .find_map(|(name, item)| match *item {
                AssocItemId::FunctionId(it) if name.as_str() == method => Some(it),
                _ => None,
            })
            .unwrap_or_else(|| panic!("`Each::{method}`"));
        crate::traits::is_impossible_associated_item(&db, krate, impl_, GenericDefId::FunctionId(item))
    })
}

#[test]
fn a_bound_the_normalized_item_fails_is_impossible() {
    for self_ty in ["Squares", "Finds", "Borrows"] {
        assert!(impossible("copied", self_ty), "`Square: Copy` fails through `{self_ty}`");
    }
}

#[test]
fn a_shared_reference_to_the_item_is_copy_through_every_impl() {
    for self_ty in ["Squares", "Finds", "Borrows"] {
        assert!(
            !impossible("borrowed_static", self_ty),
            "`&'static <{self_ty} as Each>::Item: Copy` (no binder) holds",
        );
        assert!(
            !impossible("borrowed_sized", self_ty),
            "`for<'a> &'a <{self_ty} as Each>::Item: Sized` (the sizedness fast path) holds",
        );
        assert!(
            !impossible("borrowed_named_impl_lifetime", self_ty),
            "`for<'a> &'a <{self_ty} as Each>::Item: Named` (an impl naming its lifetime) holds",
        );
        // `Mark`'s impl elides its lifetime, which lowers to `'{region error}`, not the fresh
        // early-bound parameter rustc's `fresh_lifetime` gives it.
        assert!(
            !impossible("borrowed_local_trait", self_ty),
            "`for<'a> &'a <{self_ty} as Each>::Item: Mark` (a local trait, no unknowable candidate) holds",
        );
        assert!(
            !impossible("borrowed", self_ty),
            "`for<'a> &'a <{self_ty} as Each>::Item: Copy` holds: rustdoc lists `borrowed`",
        );
    }
}

//! A method's `&Self::Output` return implies that the impl's associated type, normalized to
//! the projection it is defined as, outlives the borrow, as rustc's implied bounds of a
//! normalized signature do. Reduced from sonic-rs's `impl<I: SliceIndex<[Value]>> Index<I>
//! for Array` (vendor/sonic-rs/src/value/array.rs), which the released blaze refused with
//! E0311 "the associated type may not live long enough".

use base_db::{Db, all_crates};
use hir_def::{AssocItemId, FunctionId, nameres::crate_def_map};
use hir_expand::fixture;

use crate::{Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized
pub trait SliceIndex<T: ?Sized> {
    type Output: ?Sized;
    fn get(self, slice: &T) -> &Self::Output;
    fn get_mut(self, slice: &mut T) -> &mut Self::Output;
}

pub trait Index<Idx> {
    type Output: ?Sized;
    fn index(&self, index: Idx) -> &Self::Output;
}

pub trait IndexMut<Idx>: Index<Idx> {
    fn index_mut(&mut self, index: Idx) -> &mut Self::Output;
}

impl<T, I: SliceIndex<[T]>> Index<I> for [T] {
    type Output = I::Output;
    fn index(&self, index: I) -> &I::Output {
        index.get(self)
    }
}

impl<T, I: SliceIndex<[T]>> IndexMut<I> for [T] {
    fn index_mut(&mut self, index: I) -> &mut I::Output {
        index.get_mut(self)
    }
}

pub struct Value;

pub struct Array;

impl Array {
    fn slice(&self) -> &[Value] {
        loop {}
    }
    fn slice_mut(&mut self) -> &mut [Value] {
        loop {}
    }
}

impl<I: SliceIndex<[Value]>> Index<I> for Array {
    type Output = I::Output;
    fn index(&self, index: I) -> &Self::Output {
        Index::index(self.slice(), index)
    }
}

impl<I: SliceIndex<[Value]>> IndexMut<I> for Array {
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        IndexMut::index_mut(self.slice_mut(), index)
    }
}
"#;

fn functions(db: &Db, name: &str) -> Vec<FunctionId> {
    let mut found = Vec::new(); // alloc: a test's list
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for imp in module.scope(db).impls() {
                for &(ref item_name, item) in imp.impl_items(db).items.iter() {
                    if let AssocItemId::FunctionId(it) = item
                        && item_name.as_str() == name
                    {
                        found.push(it);
                    }
                }
            }
        }
    }
    found
}

#[test]
fn an_impls_associated_type_in_a_returned_reference_outlives_the_borrow() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        for name in ["index", "index_mut"] {
            let found = functions(&db, name);
            assert_eq!(found.len(), 2, "`{name}`: one in the impl for `[T]`, one for `Array`");
            for function in found {
                for mode in [Mode::Nll, Mode::Polonius] {
                    let result = borrowck_in_mode(&db, function.into(), mode);
                    assert_eq!(result.not_checked, None, "`{name}`");
                    assert_eq!(result.skipped, None, "`{name}`");
                    assert!(result.diagnostics.is_empty(), "`{name}`: {:?}", result.diagnostics);
                }
            }
        }
    });
}

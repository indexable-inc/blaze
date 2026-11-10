//! The type a binding has under a default binding mode (match ergonomics), as rustc gives it,
//! in editions 2021 and 2024: through a reference to a tuple inside an enum variant, nested
//! references, `&mut`, a tuple struct, a record, a slice and an array pattern.
//!
//! And what a dogfood report (2026-11-09, blaze-shader's `thread_in`) took for a wrong
//! binding type: `let Some((_, arg)) = ids.iter().find(..)` with `ids: [(&str, u32); 3]` gave
//! `arg: u32`, and `*arg` was refused (E0614), where `&u32` was expected. The pattern is not
//! the cause. The module imports `rustc_type_ir::inherent::SliceLike`, implemented for
//! `&[T; N]` with `fn iter(self)` yielding `T` by value, and a method probe takes the array
//! by reference (where that trait method applies) before it unsizes it to `[T]` (where the
//! inherent `<[T]>::iter` is), as rustc's does: so the iterator yields `(&str, u32)`, the
//! scrutinee is `Option<(&str, u32)>` and `arg: u32` is rustc's answer too. Without the trait
//! in scope the same statement binds `arg: &u32`. Both are held here.

use std::collections::BTreeMap;

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, expr_store::Body, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use rustc_type_ir::{UintTy, inherent::IntoKind as _};

use crate::{
    InferBodyId, InferenceResult,
    next_solver::{Ty, TyKind},
};

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

/// A binding's type as these tests write it: references, `u8`, `u32` and `str` spelled, any
/// other as `?`.
fn shape(ty: Ty<'_>) -> String {
    match ty.kind() {
        TyKind::Ref(_, inner, mutbl) => {
            format!("&{}{}", if mutbl.is_mut() { "mut " } else { "" }, shape(inner))
        }
        TyKind::Uint(UintTy::U8) => "u8".to_owned(),
        TyKind::Uint(UintTy::U32) => "u32".to_owned(),
        TyKind::Str => "str".to_owned(),
        _ => "?".to_owned(),
    }
}

/// Each binding of the fixture's function `name` and its type, with no diagnostic from its
/// inference.
fn bindings(fixture: &str, name: &str) -> BTreeMap<String, String> {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, name));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "`{name}`: {:?}", infer.diagnostics());
        body.bindings()
            .map(|(id, binding)| (binding.name.as_str().to_owned(), shape(infer.binding_ty(id))))
            .collect()
    })
}

/// The patterns, the same in both editions: none has a `&` pattern or a `mut` binding under
/// a default binding mode, which is where the editions differ.
const PATTERNS: &str = r#"
struct P(&'static str, u32);
struct R { name: &'static str, n: u32 }
fn f(
    shared: Option<&(&'static str, u32)>,
    twice: Option<&&(&'static str, u32)>,
    unique: Option<&mut (&'static str, u32)>,
    shared_of_unique: Option<&&mut (&'static str, u32)>,
    unique_of_shared: Option<&mut &(&'static str, u32)>,
    tuple_struct: Option<&P>,
    record: &R,
    slice: &[(u8, u32)],
    array: &mut [u32; 2],
    owned: Option<(&'static str, u32)>,
) {
    if let Some((a0, a1)) = shared {}
    let Some((_, b1)) = shared else { return };
    match twice {
        Some((_, c1)) => {}
        None => {}
    }
    if let Some((_, d1)) = unique {}
    if let Some((_, e1)) = shared_of_unique {}
    if let Some((_, g1)) = unique_of_shared {}
    if let Some(P(_, h1)) = tuple_struct {}
    let R { n: i1, .. } = record;
    if let [(j0, j1), ..] = slice {}
    let [k0, _] = array;
    if let Some((_, l1)) = owned {}
    if let Some(&(_, m1)) = shared {}
    if let Some((_, ref n1)) = owned {}
}
"#;

#[test]
fn a_binding_under_a_default_binding_mode_has_rustcs_type_in_2021_and_2024() {
    let expected = [
        // `Some((a0, a1))` of `Option<&(&str, u32)>`: the dogfood report's shape.
        ("a0", "&&str"),
        ("a1", "&u32"),
        // The same in a `let ... else`.
        ("b1", "&u32"),
        // Two references peeled: still `ref`.
        ("c1", "&u32"),
        // `&mut` makes it `ref mut`.
        ("d1", "&mut u32"),
        // Behind a `&`, a `&mut` is downgraded: once a `ref`, always a `ref`.
        ("e1", "&u32"),
        ("g1", "&u32"),
        // A tuple struct, a record, a slice and an array.
        ("h1", "&u32"),
        ("i1", "&u32"),
        ("j0", "&u8"),
        ("j1", "&u32"),
        ("k0", "&mut u32"),
        // Nothing peeled: by value.
        ("l1", "u32"),
        // An explicit `&` pattern where the mode is still by value: by value.
        ("m1", "u32"),
        // An explicit `ref`.
        ("n1", "&u32"),
    ];
    for edition in ["2021", "2024"] {
        let fixture =
            format!("//- minicore: option, sized\n//- /main.rs edition:{edition}\n{PATTERNS}");
        let found = bindings(&fixture, "f");
        for (name, ty) in expected {
            assert_eq!(
                found.get(name).map(String::as_str),
                Some(ty),
                "edition {edition}: `{name}`"
            );
        }
    }
}

/// Edition 2021 alone: a `mut` binding resets the default binding mode to by value. (In 2024
/// it is an error without `mut_ref`.)
#[test]
fn a_mut_binding_resets_the_default_binding_mode_in_2021() {
    let found = bindings(
        "//- minicore: option, sized\n//- /main.rs edition:2021\n\
         fn f(shared: Option<&(&'static str, u32)>) { if let Some((_, mut n)) = shared { n = 1; } }",
        "f",
    );
    assert_eq!(found.get("n").map(String::as_str), Some("u32"));
}

/// The dogfood report's statement, reduced: `next` for `find`, which gives the same
/// `Option<Self::Item>`. `SliceLike` is `rustc_type_ir::inherent::SliceLike` cut to its
/// `iter` and its impl for `&[T; N]`.
const ARRAY_ITER: &str = r#"
//- minicore: slice, iterator, option, sized
//- /main.rs edition:2024
pub trait SliceLike: Sized {
    type Item;
    type IntoIter: Iterator<Item = Self::Item>;
    fn iter(self) -> Self::IntoIter;
}
pub struct Copied<'a, T>(&'a [T]);
impl<'a, T> Iterator for Copied<'a, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        loop {}
    }
}
impl<'a, T, const N: usize> SliceLike for &'a [T; N] {
    type Item = T;
    type IntoIter = Copied<'a, T>;
    fn iter(self) -> Copied<'a, T> {
        loop {}
    }
}
mod with_the_trait {
    use super::SliceLike as _;
    pub fn by_value(ids: [(&'static str, u32); 3]) -> u32 {
        let Some((_, arg)) = ids.iter().next() else { return 0 };
        arg
    }
}
mod without_it {
    pub fn by_reference(ids: [(&'static str, u32); 3]) -> u32 {
        let Some((_, arg)) = ids.iter().next() else { return 0 };
        *arg
    }
}
"#;

#[test]
fn an_arrays_iter_yields_values_when_a_trait_method_takes_the_array_by_reference() {
    // `<&[T; 3] as SliceLike>::iter`, found at the array's by-reference step: `Item` is the
    // tuple, and the binding is by value. rustc refuses `*arg` here (E0614), as blaze did.
    assert_eq!(
        bindings(ARRAY_ITER, "by_value").get("arg").map(String::as_str),
        Some("u32"),
        "with the trait in scope"
    );
    // `<[T]>::iter`, found once the array is unsized: `Item` is a reference to the tuple, and
    // the binding is `ref`.
    assert_eq!(
        bindings(ARRAY_ITER, "by_reference").get("arg").map(String::as_str),
        Some("&u32"),
        "without it"
    );
}

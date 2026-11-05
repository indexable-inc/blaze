//! A trait method's `impl Trait` nested inside another type of its return type (`Option<impl
//! Sized>`, a tuple, a reference, a `Box`), with and without a body: neither the method nor a
//! caller has a type left to infer (sb-printer: rustc_type_ir's `Interner::item_bounds`).

use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceDiagnostic, InferenceResult};

/// The inference diagnostics of every function of `fixture`, trait methods included, by name.
fn diagnostics(fixture: &str) -> Vec<(String, InferenceDiagnostic)> {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let mut functions = Vec::new(); // alloc: test
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            for def in module.scope(&db).declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => functions.push(it),
                    ModuleDefId::TraitId(it) => functions.extend(
                        it.trait_items(&db).items.iter().filter_map(|(_, item)| match item {
                            AssocItemId::FunctionId(it) => Some(*it),
                            _ => None,
                        }),
                    ),
                    _ => {}
                }
            }
        }
        let mut found = Vec::new(); // alloc: test
        for function in functions {
            let name = FunctionSignature::of(&db, function).name.as_str().to_owned(); // alloc: test
            let infer = InferenceResult::of(
                &db,
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)),
            );
            found.extend(infer.diagnostics().iter().cloned().map(|it| (name.clone(), it)));
        }
        found
    })
}

#[test]
fn a_nested_impl_trait_in_a_trait_method_leaves_nothing_to_infer() {
    for (ret, value) in [
        ("impl Sized", "0u8"),
        ("Option<impl Sized>", "Some(0u8)"),
        ("(u8, impl Sized)", "(0, 0u8)"),
        ("&'static impl Sized", "&0u8"),
        ("Box<impl Sized>", "Box(0u8)"),
        ("Option<(impl Sized, Box<impl Sized>)>", "Some((0u8, Box(0u8)))"),
    ] {
        for body in [";".to_owned(), format!(" {{ {value} }}")] {
            // alloc: test
            let found = diagnostics(&format!(
                "//- minicore: sized, option\n\
                 pub struct Box<T>(T);\n\
                 pub trait D: Sized {{ fn d(self) -> {ret}{body} }}\n\
                 pub struct S;\n\
                 impl D for S {{ fn d(self) -> {ret} {{ {value} }} }}\n\
                 pub fn f<T: D>(t: T) {{ let _x = t.d(); }}\n\
                 pub fn g() {{ let _x = S.d(); }}\n"
            ));
            assert!(found.is_empty(), "`fn d(self) -> {ret}{body}`: {found:?}");
        }
    }
}

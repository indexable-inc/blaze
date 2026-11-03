//! Macros 2.0 hygiene of field and method names, as rustc's `tests/ui/hygiene/fields.rs` and
//! `impl_items.rs` check it: a field or method a `macro` names is found only by the accesses
//! and calls that expansion writes, and the caller's name of the same text finds the one the
//! caller handed in (`def_site::name_in`, `VariantFields::name_written`,
//! `inherent_value_item`). The type privacy errors of those tests are not checked here, nor,
//! of `fields.rs`, the lines that name `foo`'s private `S` and `T`: blaze resolves the `S` the
//! `macro` writes in the block it writes to the block's `struct $S` (the caller's `S`), where
//! rustc goes on to `foo::S`, a path's hygiene in a block a `macro` writes, not a field's.

use either::Either;
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ModuleDefId, expr_store::Body, hir::Expr,
    nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceDiagnostic, InferenceResult};

/// rustc's `tests/ui/hygiene/fields.rs` without its lines of `foo::S` and `foo::T`, its
/// `assert_eq!`s written as typed `let`s, with a record pattern beside the record literal.
/// `get!` writes a field name looked up in a struct of a real file, where adjusting it to the
/// struct's scope leaves it no key (`pin!`'s `__pointer` of `Pin` is the same).
const FIELDS: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

mod foo {
    pub macro m($S:ident, $x:ident) {{
        struct $S {
            $x: u32,
            x: i32,
        }

        let s = $S { $x: 0u32, x: 1i32 };
        let _: (u32, i32) = (s.$x, s.x);
        let $S { $x: a, x: b } = &s;
        let _: (&u32, &i32) = (a, b);
        s
    }}
}

fn main() {
    let s = foo::m!(S, x);
    let _: u32 = s.x;
}

struct P { x: u8 }
macro get($p:expr) { $p.x }
fn adjusted(p: P) -> u8 { get!(p) }
"#;

/// rustc's `tests/ui/hygiene/impl_items.rs`.
const IMPL_ITEMS: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

mod foo {
    struct S;
    impl S {
        fn f(&self) {}
    }

    pub macro m() {
        let _: () = S.f();
    }
}

struct S;

macro m($f:ident) {
    impl S {
        fn f(&self) -> u32 { 0 }
        fn $f(&self) -> i32 { 0 }
    }
    fn f() {
        let _: u32 = S.f();
        let _: i32 = S.$f();
    }
}

m!(f);

fn main() {
    let _: i32 = S.f();
    foo::m!();
}
"#;

/// The fixture's free function `name`, found in any module (a `macro` may have written it).
fn function(db: &base_db::Db, name: &str) -> FunctionId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's `{name}`"))
}

/// The functions of each impl of the fixture, in the order the impl declares them.
fn impls(db: &base_db::Db) -> Vec<Vec<FunctionId>> {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .map(|it| {
            it.impl_items(db)
                .items
                .iter()
                .filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                })
                .collect()
        })
        .collect()
}

/// Whether `it` is about privacy, which these ports leave out: rustc gives the tests' lines
/// "type `foo::S` is private", where blaze checks a field's privacy from the call site.
fn privacy(it: &InferenceDiagnostic) -> bool {
    matches!(
        it,
        InferenceDiagnostic::PrivateField { .. }
            | InferenceDiagnostic::PrivateAssocItem { .. }
            | InferenceDiagnostic::NoSuchField { private: Some(_), .. }
    )
}

#[test]
fn fields_a_macro_names_are_found_by_its_expansion_alone() {
    let db = fixture::db(FIELDS).into_db();
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, "main"));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        // The record literal's and the pattern's fields have the types of the fields named.
        assert!(!infer.has_type_mismatches(), "{:?}", infer.diagnostics());
        // Each field access, as (how many fields its struct has, which field).
        let fields: Vec<(usize, u32)> = body
            .exprs()
            .filter(|(_, expr)| matches!(expr, Expr::Field { .. }))
            .map(|(id, expr)| match infer.field_resolution(id) {
                Some(Either::Left(field)) => {
                    (field.parent.fields(&db).fields().len(), field.local_id.into_raw().into_u32())
                }
                other => panic!("{expr:?} resolves to {other:?}"),
            })
            .collect();
        assert_eq!(
            fields,
            [
                // `s.$x`: the caller's `x`, the `u32` field.
                (2, 0),
                // `s.x`: the `macro`'s own `x`, the `i32` field.
                (2, 1),
                // The caller's `s.x` is its own `x`, the `u32` field.
                (2, 0),
            ],
        );

        // `get!`'s `.x` of `P`, a struct of a real file: the name adjusted to `P`'s scope is
        // the plain `x`.
        let def = DefWithBodyId::FunctionId(function(&db, "adjusted"));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        let (field, _) = body
            .exprs()
            .find(|(_, expr)| matches!(expr, Expr::Field { .. }))
            .expect("`get!`'s field access");
        assert!(
            matches!(infer.field_resolution(field), Some(Either::Left(it)) if it.local_id.into_raw().into_u32() == 0),
            "{:?}",
            infer.field_resolution(field)
        );
    });
}

#[test]
fn methods_a_macro_names_are_found_by_its_expansion_alone() {
    let db = fixture::db(IMPL_ITEMS).into_db();
    blaze_query::db::attach(&db, || {
        let impls = impls(&db);
        let ours = impls.iter().find(|it| it.len() == 2).expect("the `macro`'s impl");
        let foo = impls.iter().find(|it| it.len() == 1).expect("`foo`'s impl");
        let calls = |name: &str| {
            let def = DefWithBodyId::FunctionId(function(&db, name));
            let body = Body::of(&db, def);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            let others: Vec<_> = infer.diagnostics().iter().filter(|it| !privacy(it)).collect();
            assert!(others.is_empty(), "`{name}`: {others:?}");
            assert!(!infer.has_type_mismatches(), "`{name}`: {:?}", infer.diagnostics());
            body.exprs()
                .filter(|(_, expr)| matches!(expr, Expr::MethodCall { .. }))
                .map(|(id, expr)| {
                    infer.method_resolution(id).unwrap_or_else(|| panic!("{expr:?} has no method")).0
                })
                .collect::<Vec<_>>()
        };
        // Inside the `macro`, `S.f()` is its own `f` (`u32`), `S.$f()` the caller's (`i32`).
        assert_eq!(calls("f"), [ours[0], ours[1]]);
        // The caller's `S.f()` is its own `f`, the `i32` one; `foo::m!`'s `S.f()` is
        // `foo::S::f`, an impl of a real file's item by its text.
        assert_eq!(calls("main"), [ours[1], foo[0]]);
    });
}

/// std's `OsString::push` (`library/std/src/ffi/os_str.rs`): a `macro` defined in a function's
/// body writes an impl of a trait of that block, whose method names `OsString`, an item of the
/// module around the block, and its private field `inner`. The `macro`'s def site is the block,
/// so its `OsString` is looked up there and then in the module (rustc's `expn_def_scope` and
/// `hygienic_lexical_parent`), and `buf.inner` is the field.
const OS_STRING_PUSH: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

pub struct OsString {
    inner: u32,
}

impl OsString {
    pub fn push(&mut self, s: u32) -> u32 {
        trait SpecPushTo {
            fn spec_push_to(&self, buf: &mut OsString) -> u32;
        }

        macro spec_str($T:ty) {
            impl SpecPushTo for $T {
                fn spec_push_to(&self, buf: &mut OsString) -> u32 {
                    buf.inner
                }
            }
        }
        spec_str!(u32);

        s.spec_push_to(self)
    }
}
"#;

#[test]
fn a_macro_a_block_defines_finds_the_items_of_the_block_and_its_module() {
    let db = fixture::db(OS_STRING_PUSH).into_db();
    blaze_query::db::attach(&db, || {
        let push = impls(&db).into_iter().flatten().next().expect("`OsString::push`");
        let push_def = DefWithBodyId::FunctionId(push);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(push_def));
        assert!(infer.diagnostics().is_empty(), "`push`: {:?}", infer.diagnostics());
        assert!(!infer.has_type_mismatches(), "`push`: {:?}", infer.diagnostics());
        let body = Body::of(&db, push_def);
        let spec: Vec<FunctionId> = body
            .blocks(&db)
            .flat_map(|(_, map)| {
                map.modules(&db)
                    .flat_map(|(_, module)| module.scope(&db).impls().collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            })
            .flat_map(|it| {
                it.impl_items(&db)
                    .items
                    .iter()
                    .filter_map(|&(_, item)| match item {
                        AssocItemId::FunctionId(it) => Some(it),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let [spec] = spec[..] else { panic!("one `spec_push_to`, found {spec:?}") };
        let def = DefWithBodyId::FunctionId(spec);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "`spec_push_to`: {:?}", infer.diagnostics());
        assert!(!infer.has_type_mismatches(), "`spec_push_to`: {:?}", infer.diagnostics());
        let body = Body::of(&db, def);
        let (field, _) = body
            .exprs()
            .find(|(_, it)| matches!(it, Expr::Field { .. }))
            .expect("`buf.inner`");
        assert!(
            matches!(infer.field_resolution(field), Some(Either::Left(_))),
            "`buf.inner`: {:?}",
            infer.field_resolution(field)
        );
    });
}

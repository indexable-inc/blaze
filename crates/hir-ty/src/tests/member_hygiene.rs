//! Macros 2.0 hygiene of field and method names, as rustc's `tests/ui/hygiene/fields.rs` and
//! `impl_items.rs` check it: a field or method a `macro` names is found only by the accesses
//! and calls that expansion writes, and the caller's name of the same text finds the one the
//! caller handed in (`def_site::name_in`, `VariantFields::name_written`,
//! `inherent_value_item`). The type privacy errors of those tests are not checked here. The `S`
//! the `macro` of `fields.rs` writes in the block it writes is `foo::S`, not the block's `struct
//! $S` (the caller's `S`): the block's `S` has no key, so def-site hygiene goes on past it.

use either::Either;
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ModuleDefId, expr_store::Body, hir::Expr,
    nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceDiagnostic, InferenceResult};

/// rustc's `tests/ui/hygiene/fields.rs`, its `assert_eq!`s and `let _`s written as typed `let`s,
/// with a record pattern beside the record literal.
/// `get!` writes a field name looked up in a struct of a real file, where adjusting it to the
/// struct's scope leaves it no key (`pin!`'s `__pointer` of `Pin` is the same).
const FIELDS: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

mod foo {
    struct S { x: u32 }
    struct T(u32);

    pub macro m($S:ident, $x:ident) {{
        struct $S {
            $x: u32,
            x: i32,
        }

        let s = S { x: 0 };
        let _: u32 = s.x;

        let t = T(0);
        let _: u32 = t.0;

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

/// Whether `it` is about privacy, which these ports leave out (rustc gives the tests' lines
/// "type `foo::S` is private", a type privacy blaze does not check); the def-site privacy of
/// fields and methods is checked by `privacy_is_checked_from_the_def_site_of_the_name`.
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
        assert!(infer.diagnostics().iter().all(privacy), "{:?}", infer.diagnostics());
        // The record literals' and the pattern's fields have the types of the fields named.
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
                // `s.x` of `foo::S` and `t.0` of `foo::T`, each their one field.
                (1, 0),
                (1, 0),
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
                    infer
                        .method_resolution(id)
                        .unwrap_or_else(|| panic!("{expr:?} has no method"))
                        .0
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
        let (field, _) =
            body.exprs().find(|(_, it)| matches!(it, Expr::Field { .. })).expect("`buf.inner`");
        assert!(
            matches!(infer.field_resolution(field), Some(Either::Left(_))),
            "`buf.inner`: {:?}",
            infer.field_resolution(field)
        );
    });
}

/// rustc's `tests/ui/hygiene/trait_items-2.rs`: a trait a `macro` writes has a method `f` of its
/// own and the caller's `$f`, and each call finds the one its context names, through the traits
/// in scope (`trait_item_name`, `trait_value_item`, rustc's `find_by_ident_and_namespace`).
const TRAIT_ITEMS_2: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

macro m($T:ident, $f:ident) {
    pub trait $T {
        fn f(&self) -> u32 { 0 }
        fn $f(&self) -> i32 { 0 }
    }
    impl $T for () {}

    let _: u32 = ().f();
    let _: i32 = ().$f();
}

fn main() {
    m!(T, f);
    let _: i32 = ().f();
}
"#;

/// The functions of each trait the blocks of `def`'s body declare, in declaration order.
fn block_trait_fns(db: &base_db::Db, def: DefWithBodyId) -> Vec<FunctionId> {
    Body::of(db, def)
        .blocks(db)
        .flat_map(|(_, map)| {
            map.modules(db)
                .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .filter_map(|def| match def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        })
        .flat_map(|it| {
            it.trait_items(db)
                .items
                .iter()
                .filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn trait_methods_a_macro_names_are_found_by_its_expansion_alone() {
    let db = fixture::db(TRAIT_ITEMS_2).into_db();
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, "main"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        assert!(!infer.has_type_mismatches(), "{:?}", infer.diagnostics());
        let [own, callers] = block_trait_fns(&db, def)[..] else {
            panic!("the trait's two `f`s: {:?}", block_trait_fns(&db, def))
        };
        let body = Body::of(&db, def);
        let calls: Vec<FunctionId> = body
            .exprs()
            .filter(|(_, expr)| matches!(expr, Expr::MethodCall { .. }))
            .map(|(id, expr)| {
                infer.method_resolution(id).unwrap_or_else(|| panic!("{expr:?} has no method")).0
            })
            .collect();
        // The `macro`'s `().f()` is its own `f` (`u32`), its `().$f()` the caller's (`i32`), and
        // the caller's `().f()` the caller's.
        assert_eq!(calls, [own, callers, callers]);
    });
}

/// rustc's `tests/ui/hygiene/trait_items.rs`, its paths written `crate::` for the fixture's
/// edition: the traits in scope of a method call `baz::m` writes are those of rustc's hygienic
/// walk (`traits_in_scope`, `hygienic_lexical_parent`): the call's block, then `baz`, the
/// `macro`'s def site, which has no `T`. So `().f()` finds no method (E0599), while `bar::m`'s
/// call in `baz` reaches `bar`, whose glob imports `T`.
const TRAIT_ITEMS: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

mod foo {
    pub trait T {
        fn f(&self) {}
    }
    impl T for () {}
}

mod bar {
    use crate::foo::*;
    pub macro m() { ().f() }
    fn f() { crate::baz::m!(); }
}

mod baz {
    pub macro m() { ().f() }
    fn f() { crate::bar::m!(); }
}
"#;

#[test]
fn the_traits_in_scope_of_a_macros_call_are_its_def_sites() {
    let db = fixture::db(TRAIT_ITEMS).into_db();
    blaze_query::db::attach(&db, || {
        let bodies: Vec<FunctionId> = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            })
            .collect();
        let unresolved = |f: FunctionId| {
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(f.into()));
            infer
                .diagnostics()
                .iter()
                .filter(|it| matches!(it, InferenceDiagnostic::UnresolvedMethodCall { .. }))
                .count()
        };
        let in_module = |f: FunctionId| {
            let module = hir_def::HasModule::module(&f, &db);
            module.name(&db).map(|it| it.as_str().to_owned())
        };
        let mut found: Vec<(Option<String>, usize)> =
            bodies.into_iter().map(|f| (in_module(f), unresolved(f))).collect();
        found.sort();
        // `bar::f` calls `baz::m`, whose `().f()` finds no `T` (rustc's E0599 at 18:24); `baz::f`
        // calls `bar::m`, whose call sees `bar`'s `T`.
        assert_eq!(found, [(Some("bar".to_owned()), 1), (Some("baz".to_owned()), 0)]);
    });
}

/// rustc's `tests/ui/hygiene/specialization.rs`: the method `f` an impl a `macro` writes for a
/// trait of a real file implements the trait's `f`, whose default body must not run. Its key
/// adjusted to the trait's scope is no key (`implemented_name`, rustc's `check_trait_item`).
const SPECIALIZATION: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

trait Tr {
    fn f(&self) -> &'static str {
        "This shouldn't happen"
    }
}

pub macro m($t:ty) {
    impl Tr for $t {
        fn f(&self) -> &'static str {
            "Run me"
        }
    }
}

struct S;
m!(S);

fn main() {
    let _ = S.f();
}
"#;

#[test]
fn a_macros_impl_of_a_real_files_trait_overrides_its_default() {
    let db = fixture::db(SPECIALIZATION).into_db();
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, "main"));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        let (call, _) = body
            .exprs()
            .find(|(_, expr)| matches!(expr, Expr::MethodCall { .. }))
            .expect("`S.f()`");
        let (func, args) = infer.method_resolution(call).expect("`S.f()` resolves");
        let env = crate::ParamEnvAndCrate {
            param_env: crate::next_solver::ParamEnv::empty(),
            krate: fixture::test_crate(&db),
        };
        let (found, _) = crate::db::HirDatabase::lookup_impl_method(&db, env, func, args);
        let impls = impls(&db);
        let [ours] = &impls[..] else { panic!("the `macro`'s impl: {impls:?}") };
        assert_eq!(found, Either::Left(ours[0]), "`S.f()` runs the impl's `f`");
    });
}

/// Associated items named through paths (`T::A`, `<T as Tr>::A`, `Self::A`, `T::f`,
/// `<T>::f`, `<() as Tr>::f`, `S::f`) and bindings (`Tr<A = ..>`), on a trait and an inherent
/// impl a `macro` writes with an item `A` and an `f` of its own and the caller's `$A` and `$f`:
/// each path and binding finds the item its context names (rustc's `hygienic_eq` in
/// `find_bound_for_assoc_item`, `probe_assoc_item` and `lower_assoc_item_constraint`), as rustc
/// accepts it; a lookup by text would give the `u8` item where the `u16` one is named, or the
/// reverse. rustc's `assoc_ty_bindings.rs`, whose `macro` names a real file's trait's items, is
/// the case of no key.
const ASSOC_PATHS: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

macro m($Tr:ident, $A:ident, $f:ident, $S:ident) {
    pub trait $Tr {
        type A;
        type $A;
        fn f() -> u8;
        fn $f() -> u16;
    }
    impl $Tr for () {
        type A = u8;
        type $A = u16;
        fn f() -> u8 { let _: Self::A = 0u8; 0 }
        fn $f() -> u16 { let _: Self::$A = 0u16; 0 }
    }
    pub struct $S;
    impl $S {
        fn f() -> u8 { 0 }
        fn $f() -> u16 { 0 }
    }
    pub fn own<T: $Tr>(a: T::A, b: T::$A) -> (<T as $Tr>::A, <T as $Tr>::$A) {
        let _: (u8, u16) = (<() as $Tr>::f(), <()>::$f());
        let _: (<() as $Tr>::A, <() as $Tr>::$A) = (0u8, 0u16);
        let _: (u8, u16) = (<()>::f(), <() as $Tr>::$f());
        let _: (u8, u16) = ($S::f(), <$S>::$f());
        (a, b)
    }
    pub fn bound<T: $Tr<A = u8, $A = u16>>(a: <T as $Tr>::A, b: <T as $Tr>::$A) -> (u8, u16) {
        let _: (u8, u16) = (T::f(), T::$f());
        (a, b)
    }
}

m!(Tr, A, f, S);

fn caller<T: Tr>(a: T::A) -> <T as Tr>::A {
    let _: u16 = T::f();
    let _: u16 = <T>::f();
    let _: u16 = <()>::f();
    let _: u16 = <() as Tr>::f();
    let _: <() as Tr>::A = 0u16;
    let _: u16 = S::f();
    a
}

fn caller_bound<T: Tr<A = u16>>(a: <T as Tr>::A) -> u16 { a }
"#;

#[test]
fn associated_items_a_macro_names_through_paths_are_found_by_its_expansion_alone() {
    use rustc_type_ir::TypeVisitableExt as _;
    let db = fixture::db(ASSOC_PATHS).into_db();
    blaze_query::db::attach(&db, || {
        let mut defs: Vec<FunctionId> = ["own", "bound", "caller", "caller_bound"]
            .into_iter()
            .map(|it| function(&db, it))
            .collect();
        defs.extend(impls(&db).concat());
        assert_eq!(defs.len(), 8, "four functions and the impls' four");
        for def in defs {
            let name = FunctionSignature::of(&db, def).name.as_str().to_owned();
            let sig = crate::db::HirDatabase::callable_item_signature(&db, def.into());
            assert!(!sig.skip_binder().references_error(), "`{name}`'s signature: {sig:?}");
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def.into()));
            assert!(infer.diagnostics().is_empty(), "`{name}`: {:?}", infer.diagnostics());
            assert!(!infer.has_type_mismatches(), "`{name}` has a type mismatch");
        }
    });
}
/// rustc's `tests/ui/hygiene/nested_macro_privacy.rs`: the private field `i` of a struct a
/// `macro` writes in a module is private to the caller, and readable by the `pub macro` written
/// beside it, wherever that is called (`def_site::privacy_scope`).
const NESTED_MACRO_PRIVACY: &str = r#"
//- minicore: sized, default, derive
#![feature(decl_macro)]

macro n($foo:ident, $S:ident, $i:ident, $m:ident) {
    mod $foo {
        #[derive(Default)]
        pub struct $S { $i: u32 }
        pub macro $m($e:expr) { $e.$i }
    }
}

n!(foo, S, i, m);

fn main() {
    use foo::{S, m};
    S::default().i;
    m!(S::default());
}
"#;

/// The method version of `NESTED_MACRO_PRIVACY` (no rustc ui test; rustc 2026-10-26 gives
/// exactly E0624 "method `f` is private" at the first call).
const NESTED_MACRO_METHOD_PRIVACY: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

macro n($foo:ident, $S:ident, $f:ident, $m:ident) {
    mod $foo {
        pub struct $S;
        impl $S {
            fn $f(&self) -> u32 { 0 }
        }
        pub macro $m($e:expr) { $e.$f() }
    }
}

n!(foo, S, f, m);

fn main() {
    use foo::{S, m};
    S.f();
    let _: u32 = m!(S);
}
"#;

/// The field privacy lines of rustc's `tests/ui/hygiene/fields.rs` (`t.0` of `foo`'s private
/// `T(u32)`), with a named field, a record literal and a record pattern beside it, the types
/// made public so that rustc's type privacy errors are not the point: rustc 2026-10-26 accepts
/// the `macro`'s reads and gives E0616 for each of `outside`'s. The record paths are written
/// `crate::foo::U`: blaze does not resolve a plain `U` written in the block the `macro` writes
/// (a struct path's hygiene, not this rule).
const FIELDS_OF_THE_DEF_SITE: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

mod foo {
    pub struct T(u32);
    pub struct U { x: u32 }
    pub fn t() -> T { T(0) }
    pub fn u() -> U { U { x: 0 } }

    pub macro m() {{
        let _: u32 = t().0;
        let _: u32 = u().x;
        let crate::foo::U { x } = u();
        let _: u32 = x;
        let _ = crate::foo::U { x: 1 };
    }}
}

fn main() {
    foo::m!();
}

fn outside() {
    let _: u32 = foo::t().0;
    let _: u32 = foo::u().x;
}
"#;

/// `NESTED_MACRO_PRIVACY` with the module in a function's block: the `macro`'s module is a child
/// of the block's, which no walk out of the caller's module passes (rustc 2026-10-26 gives
/// exactly one E0616, at the first access).
const BLOCK_MACRO_PRIVACY: &str = r#"
//- minicore: sized
#![feature(decl_macro)]

fn main() {
    mod foo {
        pub struct S { x: u32 }
        pub fn s() -> S { S { x: 0 } }
        pub macro m($e:expr) { $e.x }
    }
    let _: u32 = foo::s().x;
    let _: u32 = foo::m!(foo::s());
}
"#;

/// The field accesses of the fixture's function `name`, in the order its body has them, and
/// its inference diagnostics.
fn accesses_and_diagnostics<'db>(
    db: &'db base_db::Db,
    name: &str,
) -> (Vec<hir_def::hir::ExprId>, &'db [InferenceDiagnostic]) {
    let def = DefWithBodyId::FunctionId(function(db, name));
    let body = Body::of(db, def);
    let fields = body
        .exprs()
        .filter(|(_, expr)| matches!(expr, Expr::Field { .. }))
        .map(|(id, _)| id)
        .collect();
    (fields, InferenceResult::of(db, InferBodyId::DefWithBodyId(def)).diagnostics())
}

#[test]
fn privacy_is_checked_from_the_def_site_of_the_name() {
    let db = fixture::db(NESTED_MACRO_PRIVACY).into_db();
    blaze_query::db::attach(&db, || {
        let (fields, diagnostics) = accesses_and_diagnostics(&db, "main");
        // `S::default().i` alone: `m!`'s `$e.$i` is checked from `foo`, where `m` is defined.
        assert!(
            matches!(diagnostics, [InferenceDiagnostic::PrivateField { expr, .. }] if *expr == fields[0]),
            "{diagnostics:?}"
        );
    });

    let db = fixture::db(NESTED_MACRO_METHOD_PRIVACY).into_db();
    blaze_query::db::attach(&db, || {
        let (_, diagnostics) = accesses_and_diagnostics(&db, "main");
        assert!(
            matches!(diagnostics, [InferenceDiagnostic::PrivateAssocItem { .. }]),
            "{diagnostics:?}"
        );
    });

    let db = fixture::db(FIELDS_OF_THE_DEF_SITE).into_db();
    blaze_query::db::attach(&db, || {
        let (_, diagnostics) = accesses_and_diagnostics(&db, "main");
        assert!(diagnostics.is_empty(), "`foo::m!`: {diagnostics:?}");
        let (fields, diagnostics) = accesses_and_diagnostics(&db, "outside");
        let private: Vec<_> = diagnostics
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::PrivateField { expr, .. } => Some(*expr),
                _ => None,
            })
            .collect();
        assert_eq!(private, fields, "`outside`: {diagnostics:?}");
    });

    let db = fixture::db(BLOCK_MACRO_PRIVACY).into_db();
    blaze_query::db::attach(&db, || {
        let (fields, diagnostics) = accesses_and_diagnostics(&db, "main");
        assert!(
            matches!(diagnostics, [InferenceDiagnostic::PrivateField { expr, .. }] if *expr == fields[0]),
            "{diagnostics:?}"
        );
    });
}

//! Macro hygiene of paths in bodies, as rustc's `tests/ui/hygiene` checks it: what each path of
//! each function resolves to, with the syntax context its first segment was written with.
//!
//! A function is told apart by its return type, as the rustc tests do (`fn f() -> u32` and
//! `fn f() -> i32` are two `f`s). Each test names the rustc test it ports.
//!
//! [`check_generic_params`] does the same for the generic parameters of each function's
//! signature: which parameter each type, lifetime and const in it names.

use std::fmt::Write;

use expect_test::{Expect, expect};
use hir_expand::fixture;
use syntax::view::{self, AstNode as _};

use crate::{
    DefWithBodyId, FunctionId, Lookup, ModuleDefId,
    expr_store::{Body, scope::ExprScopes},
    hir::Expr,
    nameres::{DefMap, crate_def_map},
    resolver::{ValueNs, resolver_for_scope},
    src::HasSource as _,
    test_db::{TestDB, TestDBExt},
};

use super::super::super::*;

/// Every function of the crate, in its modules and in the blocks of their bodies, and for each
/// path of its body what it resolves to in the value namespace.
fn check_resolutions(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: &Expect) {
    let db = fixture::db(ra_fixture).into_db();
    let krate = db.fetch_test_crate();
    let mut functions = Vec::new();
    collect_functions(&db, crate_def_map(&db, krate), &mut functions);
    let mut out = String::new();
    for function in functions {
        let def = DefWithBodyId::from(function);
        let body = Body::of(&db, def);
        let scopes = ExprScopes::of(&db, def);
        let mut lines = Vec::new();
        for (expr, it) in body.exprs() {
            let Expr::Path(path) = it else { continue };
            let Some(mod_path) = path.mod_path() else { continue };
            let resolver = resolver_for_scope(&db, def, scopes.scope_for(expr));
            let hygiene = body.expr_path_hygiene(expr);
            let to = match resolver.resolve_path_in_value_ns_fully(&db, path, hygiene) {
                Some(ValueNs::FunctionId(it)) => describe(&db, it),
                Some(ValueNs::StaticId(it)) => {
                    let src = it.lookup(&db).source_view(&db).value;
                    let init =
                        src.body().map_or_else(String::new, |it| it.syntax().text().to_string());
                    format!("static = {init}")
                }
                Some(ValueNs::StructId(it)) => {
                    let src = it.lookup(&db).source_view(&db).value;
                    let name = view::HasName::name(&src)
                        .map_or_else(String::new, |it| it.syntax().text().to_string());
                    format!("struct {name}")
                }
                Some(ValueNs::LocalBinding(it)) => format!("local {}", body[it].name.as_str()),
                Some(other) => format!("{other:?}"),
                None => "unresolved".to_owned(),
            };
            lines.push(format!("    {} -> {to}", mod_path.display_verbatim(&db)));
        }
        if !lines.is_empty() {
            writeln!(out, "{}:", describe(&db, function)).unwrap();
            for line in lines {
                writeln!(out, "{line}").unwrap();
            }
        }
    }
    expect.assert_eq(&out);
}

fn collect_functions(db: &TestDB, map: &DefMap, out: &mut Vec<FunctionId>) {
    for (_, module) in map.modules(db) {
        for decl in module.scope(db).declarations() {
            let ModuleDefId::FunctionId(it) = decl else { continue };
            out.push(it);
            let body = Body::of(db, it.into());
            for (_, block) in body.blocks(db) {
                collect_functions(db, block, out);
            }
        }
    }
}

/// `fn name -> ret`, the return type as written (`()` for none).
fn describe(db: &TestDB, function: FunctionId) -> String {
    let src = function.lookup(db).source_view(db).value;
    let name =
        view::HasName::name(&src).map_or_else(String::new, |it| it.syntax().text().to_string());
    let ret = src
        .ret_type()
        .and_then(|it| it.ty())
        .map_or_else(|| "()".to_owned(), |it| it.syntax().text().to_string());
    format!("fn {name} -> {ret}")
}

#[test]
fn two_expansions_of_a_macro_each_define_and_name_their_own_static() {
    // std's `weak!` on macOS (`sys/pal/unix/weak/dlsym.rs`), called twice in one block by
    // `get_posix_spawn_addchdir`: each expansion's `static DLSYM` is keyed by its own context,
    // so the two don't collide and each `&DLSYM` names its own. rustc prints `1 2`.
    check_resolutions(
        r#"
#![feature(decl_macro)]
macro weak($name:ident = $v:expr) {
    static DLSYM: u32 = $v;
    let $name = &DLSYM;
}
fn main() {
    weak!(a = 1);
    weak!(b = 2);
    (a, b);
}
"#,
        &expect![[r#"
            fn main -> ():
                DLSYM -> static = 1
                DLSYM -> static = 2
                a -> local a
                b -> local b
        "#]],
    );
}

#[test]
fn an_item_a_macro_writes_is_not_seen_by_its_caller() {
    // rustc's tests/ui/hygiene/rustc-macro-transparency.rs: a name an opaque `macro` writes
    // is its own, a semi-opaque or transparent one's is the caller's too. Items are keyed by
    // `normalize_to_macros_2_0`, locals by `normalize_to_macro_rules`.
    check_resolutions(
        r#"
#![feature(decl_macro, rustc_attrs)]
#[rustc_macro_transparency = "transparent"]
macro transparent() {
    struct Transparent;
    let transparent = 0;
}
#[rustc_macro_transparency = "semiopaque"]
macro semiopaque() {
    struct SemiOpaque;
    let semiopaque = 0;
}
#[rustc_macro_transparency = "opaque"]
macro opaque() {
    struct Opaque;
    let opaque = 0;
}
fn main() {
    transparent!();
    semiopaque!();
    opaque!();
    Transparent;
    SemiOpaque;
    Opaque;
    transparent;
    semiopaque;
    opaque;
}
"#,
        &expect![[r#"
            fn main -> ():
                Transparent -> struct Transparent
                SemiOpaque -> struct SemiOpaque
                Opaque -> unresolved
                transparent -> local transparent
                semiopaque -> unresolved
                opaque -> unresolved
        "#]],
    );
}

#[test]
fn a_macro_sees_its_def_site_and_the_names_it_is_handed() {
    // rustc's tests/ui/hygiene/lexical.rs: `baz::f` resolves at the def site, `$f` where it is
    // written, and the `fn $f` the macro declares in a block is the caller's name.
    check_resolutions(
        r#"
#![feature(decl_macro)]
mod bar {
    mod baz {
        pub fn f() -> () {}
    }
    pub macro m($f:ident) {
        baz::f();
        let _: i32 = $f();
        {
            fn $f() -> u32 { 0 }
            let _: u32 = $f();
        }
    }
}
fn main() {
    fn f() -> i32 { 0 }
    bar::m!(f);
}
"#,
        &expect![[r#"
            fn main -> ():
                baz::f -> fn f -> ()
                f -> fn f -> i32
                f -> fn f -> u32
        "#]],
    );
}

#[test]
fn a_name_a_macro_is_handed_is_not_the_macros_own() {
    // stdgaps' limit: a `fn $f` a `macro` declares is named by the caller, so the macro's own
    // `f` does not see it and goes on to its def site, while the caller's `f` does.
    check_resolutions(
        r#"
#![feature(decl_macro)]
mod defs {
    pub fn f() -> i32 { 0 }
    pub macro m($f:ident) {
        fn $f() -> u32 { 0 }
        fn g() -> bool { f(); true }
    }
}
mod user {
    crate::defs::m!(f);
    fn h() -> () { f(); }
}
"#,
        &expect![[r#"
            fn h -> ():
                f -> fn f -> u32
            fn g -> bool:
                f -> fn f -> i32
        "#]],
    );
}

#[test]
fn a_macro_rules_call_inside_a_macro_stays_hygienic() {
    // rustc's tests/ui/hygiene/legacy_interaction.rs, the `macro_rules!` in this crate: what
    // the `macro_rules!` writes inside `m2` is `m2`'s, so `m2`'s `f()` finds the `macro_rules!`'s
    // `f` and its `g()` finds `m2`'s `g`; the caller's `f` and `g` are its own.
    check_resolutions(
        r#"
#![feature(decl_macro)]
#[macro_export]
macro_rules! m {
    () => {
        fn f() -> u8 { 0 }
        g();
    }
}
mod def_site {
    pub macro m2() {
        crate::m!();
        f();
        fn g() -> u16 { 0 }
    }
}
mod use_site {
    fn test() -> () {
        fn f() -> bool { true }
        fn g() -> char { 'g' }
        crate::def_site::m2!();
        f();
        g();
    }
}
"#,
        &expect![[r#"
            fn test -> ():
                g -> fn g -> u16
                f -> fn f -> u8
                f -> fn f -> bool
                g -> fn g -> char
        "#]],
    );
}

#[test]
fn a_path_with_more_segments_is_no_local() {
    // rustc's tests/ui/hygiene/hygiene-dodging-1.rs.
    check_resolutions(
        r#"
mod x {
    pub fn g() -> usize { 14 }
}
fn main() {
    let x = 9;
    x;
    x::g();
}
"#,
        &expect![[r#"
            fn main -> ():
                x -> local x
                x::g -> fn g -> usize
        "#]],
    );
}

/// The dumps of the block def maps of `main`'s body, and for each keyed name of them the id of
/// its key's context with the position of the macro call that wrote it (rule 25's check). With
/// `first`, those calls are interned and expanded in the reverse of their order before the
/// body is lowered, so their contexts are numbered the other way round.
fn main_blocks(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    first: Option<&[hir_expand::MacroCallLoc]>,
) -> (String, Vec<(u32, usize)>, Vec<hir_expand::MacroCallLoc>) {
    use hir_expand::{HirFileId, Intern as _, Lookup as _};

    let db = fixture::db(ra_fixture).into_db();
    if let Some(locs) = first {
        for loc in locs.iter().rev() {
            let call = loc.clone().intern(&db);
            HirFileId::from(call).root(&db);
        }
    }
    let krate = db.fetch_test_crate();
    let mut functions = Vec::new();
    collect_functions(&db, crate_def_map(&db, krate), &mut functions);
    let main = functions
        .into_iter()
        .find(|&it| describe(&db, it).starts_with("fn main "))
        .expect("the fixture has a `fn main`");
    let body = Body::of(&db, main.into());
    let calls = body.store.macro_calls().to_vec();
    let mut dump = String::new();
    let mut keys = Vec::new();
    for (_, map) in body.blocks(&db) {
        dump.push_str(&map.dump(&db));
        for (name, _) in map.module(&db, map.root_module_id()).scope(&db).entries() {
            let Some(key) = name.key() else { continue };
            let expn = hir_expand::MacroCallId::from(
                key.outer_expn(&db).expect("a keyed name's context has a mark"),
            );
            let at =
                calls.iter().position(|&it| it == expn).expect("a call of `main`'s body wrote it");
            keys.push((key.into_u32(), at));
        }
    }
    let locs = calls.iter().map(|it| it.lookup(&db).clone()).collect();
    (dump, keys, locs)
}

#[test]
fn hygienic_names_of_one_text_list_in_the_order_they_were_bound() {
    // Rule 25: two hygienic `S`s in one block, a struct from `a!` and a function from `b!`,
    // list in the order the block bound them whatever their contexts' ids are. The second build
    // interns the calls' contexts in the reverse order; ordered by context id the dump swaps.
    let fixture = r#"
#![feature(decl_macro)]
macro a() { struct S; }
macro b() { fn S() {} }
fn main() {
    a!();
    b!();
}
"#;
    let (first, first_keys, locs) = main_blocks(fixture, None);
    let (second, second_keys, _) = main_blocks(fixture, Some(&locs));
    let by_call = |keys: &[(u32, usize)]| {
        let mut keys = keys.to_vec();
        keys.sort_by_key(|&(_, at)| at);
        keys.dedup();
        keys.iter().map(|&(id, _)| id).collect::<Vec<_>>()
    };
    let (ids, reversed) = (by_call(&first_keys), by_call(&second_keys));
    assert_eq!(ids.len(), 2, "two keyed `S`s: {first_keys:?}");
    assert!(
        ids[0] < ids[1] && reversed[0] > reversed[1],
        "the second build numbers the contexts the other way: {ids:?} {reversed:?}"
    );
    expect![[r#"
        (block scope)
        - S : type value
        - S : value

        crate
        - a : macro!
        - b : macro!
        - main : value
    "#]]
    .assert_eq(&first);
    assert_eq!(first, second, "the dump of the same tree, built twice");
}

/// rustc's tests/ui/hygiene/items.rs: a `macro` that defines a module `foo` and is handed a
/// module name `foo` defines two modules, and each segment of a path finds the module and the
/// function written with its own context (rustc's `normalize_to_macros_2_0_and_adjust` of each
/// segment in the module the path reached).
#[test]
fn each_segment_of_a_path_finds_the_item_written_with_its_context() {
    check_resolutions(
        r#"
//- /main.rs crate:main
#![feature(decl_macro)]
pub macro m($foo:ident, $f:ident, $e:expr) {
    mod foo {
        pub fn f() -> u32 { 0 }
        pub fn $f() -> u64 { 0 }
    }
    mod $foo {
        pub fn f() -> i32 { 0 }
        pub fn $f() -> i64 { 0  }
    }
    let _: u32 = foo::f();
    let _: u64 = foo::$f();
    let _: i32 = $foo::f();
    let _: i64 = $foo::$f();
    let _: i64 = $e;
}

fn main() {
    m!(foo, f, foo::f());
    let _: i64 = foo::f();
}
"#,
        &expect![[r#"
            fn main -> ():
                foo::f -> fn f -> u32
                foo::f -> fn f -> u64
                foo::f -> fn f -> i32
                foo::f -> fn f -> i64
                foo::f -> fn f -> i64
                foo::f -> fn f -> i64
        "#]],
    );
}

/// A `macro_rules!` a `macro` defines binds its name keyed by the context it is written with
/// (rustc's `MacroRulesDecl::ident`, an `IdentKey`): the `macro`'s own `inner` is not its
/// caller's, while one whose name the caller handed it is.
#[test]
fn a_macro_rules_a_macro_defines_is_named_with_its_context() {
    check_resolutions(
        r#"
//- /main.rs crate:main
#![feature(decl_macro)]
fn outer() -> u32 { 0 }
fn own() -> u8 { 0 }
fn handed() -> u16 { 0 }
macro m($name:ident) {
    macro_rules! inner { () => { own } }
    macro_rules! $name { () => { handed } }
    let _ = inner!();
    let _ = $name!();
}
macro_rules! inner { () => { outer } }
fn main() {
    m!(caller);
    let _ = inner!();
    let _ = caller!();
}
"#,
        &expect![[r#"
            fn main -> ():
                own -> fn own -> u8
                handed -> fn handed -> u16
                outer -> fn outer -> u32
                handed -> fn handed -> u16
        "#]],
    );
}

/// An `extern crate` a `macro` writes binds its name keyed by the context it is written with,
/// in the module's scope and the extern prelude (rustc's `IdentKey` for both): the `macro`'s
/// own `own` is not its caller's, while the name the caller handed it is.
#[test]
fn an_extern_crate_a_macro_writes_is_named_with_its_context() {
    check_resolutions(
        r#"
//- /main.rs crate:main deps:dep
#![feature(decl_macro)]
macro m($name:ident) {
    extern crate dep as own;
    extern crate dep as $name;
    fn in_m() {
        own::f();
        $name::f();
    }
}
m!(handed);
fn main() {
    handed::f();
    own::f();
}
//- /dep.rs crate:dep
pub fn f() -> u8 { 0 }
"#,
        &expect![[r#"
            fn in_m -> ():
                own::f -> fn f -> u8
                handed::f -> fn f -> u8
            fn main -> ():
                handed::f -> fn f -> u8
                own::f -> unresolved
        "#]],
    );
}

/// rustc's tests/ui/hygiene/globs.rs, its first part: a glob binds each name it imports with
/// the glob's marks added (rustc's `reverse_glob_adjust`), so the names the `macro` writes find
/// what its own glob imports and the names its caller handed it find what the caller's does.
#[test]
fn a_glob_a_macro_writes_imports_for_the_macros_names() {
    check_resolutions(
        r#"
//- /main.rs crate:main edition:2015
#![feature(decl_macro)]
mod foo {
    pub fn f() {}
}
mod bar {
    pub fn g() {}
}
macro m($($t:tt)*) {
    $($t)*
    use foo::*;
    f();
    g();
}
fn main() {
    m! {
        use bar::*;
        g();
        f();
    }
}
"#,
        &expect![[r#"
            fn main -> ():
                g -> fn g -> ()
                f -> unresolved
                f -> fn f -> ()
                g -> unresolved
        "#]],
    );
}

/// rustc's tests/ui/hygiene/globs.rs, its second part: `use super::*` in modules that nested
/// `macro`s write imports the names of the module around them as each `macro`'s own names see
/// them, and a name handed in from outside a glob's expansion is not imported for it. In `mod
/// test2`, which calls the nested `macro`s from a child module, `use super::*` and `super::$j`
/// reach `foo` by hygienic `super` (rustc's `resolve_self`: `test2`'s `mod` is not written by
/// the `macro`, so the walk goes on to the module defining it), as `globs.stderr` has it: only
/// the `f` handed in at `test2` (its 66:17) is not found.
#[test]
fn globs_in_modules_nested_macros_write() {
    check_resolutions(
        r#"
//- /main.rs crate:main edition:2015
#![feature(decl_macro)]
n!(f);
macro n($i:ident) {
    mod foo {
        pub fn $i() -> u32 { 0 }
        pub fn f() {}

        mod test {
            use super::*;
            fn g() {
                let _: u32 = $i();
                let _: () = f();
            }
        }

        macro n($j:ident) {
            mod test {
                use super::*;
                fn g() {
                    let _: u32 = $i();
                    let _: () = f();
                    $j();
                }
            }
        }
        macro n_with_super($j:ident) {
            mod test {
                use super::*;
                fn g() {
                    let _: u32 = $i();
                    let _: () = f();
                    super::$j();
                }
            }
        }

        n!(f);
        n_with_super!(f);
        mod test2 {
            super::n! {
                f
            }
            super::n_with_super! {
                f
            }
        }
    }
}
"#,
        &expect![[r#"
            fn g -> ():
                f -> fn f -> u32
                f -> fn f -> ()
            fn g -> ():
                f -> fn f -> u32
                f -> fn f -> ()
                f -> unresolved
            fn g -> ():
                f -> fn f -> u32
                f -> fn f -> ()
                super::f -> fn f -> ()
            fn g -> ():
                f -> fn f -> u32
                f -> fn f -> ()
                f -> unresolved
            fn g -> ():
                f -> fn f -> u32
                f -> fn f -> ()
                super::f -> fn f -> ()
        "#]],
    );
}

/// Every function of the crate with its generic parameters and, for each type, lifetime and
/// const its signature writes, the parameter it names. A parameter is `name#index` in its list
/// (lifetimes and type or const parameters counted apart), `name*#index` when its name is keyed
/// ([`crate::nameres::param_name`]: an opaque `macro` wrote it).
fn check_generic_params(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: &Expect) {
    use crate::{
        GenericDefId, LifetimeParamId, TypeOrConstParamId,
        hir::{
            generics::{GenericParams, WherePredicate},
            type_ref::{LifetimeRefId, TypeBound, TypeRef, TypeRefId},
        },
        resolver::{HasResolver as _, LifetimeNs, Resolver, TypeNs},
        signatures::FunctionSignature,
    };
    use hir_expand::name::Name;

    struct Cx<'a> {
        db: &'a TestDB,
        resolver: Resolver<'a>,
        signature: &'a FunctionSignature,
        def: GenericDefId,
    }
    fn label(name: &Name, index: u32) -> String {
        let keyed = if name.key().is_some() { "*" } else { "" };
        format!("{}{keyed}#{index}", name.as_str())
    }
    impl Cx<'_> {
        fn params(&self) -> &GenericParams {
            &self.signature.generic_params
        }
        fn type_or_const(&self, id: TypeOrConstParamId) -> String {
            assert_eq!(id.parent, self.def, "a parameter of the function's own list");
            let name = self.params()[id.local_id].name().expect("a named parameter");
            label(name, id.local_id.into_raw().into_u32())
        }
        fn lifetime_param(&self, id: LifetimeParamId) -> String {
            assert_eq!(id.parent, self.def, "a parameter of the function's own list");
            label(&self.params()[id.local_id].name, id.local_id.into_raw().into_u32())
        }
        fn lifetime(&self, lifetime: Option<LifetimeRefId>) -> String {
            let Some(lifetime) = lifetime else { return "'_".to_owned() };
            match self.resolver.resolve_lifetime(&self.signature.store[lifetime]) {
                Some(LifetimeNs::LifetimeParam(id)) => self.lifetime_param(id),
                Some(LifetimeNs::Static) => "'static".to_owned(),
                None => "unresolved".to_owned(),
            }
        }
        fn ty(&self, ty: TypeRefId) -> String {
            let store = &self.signature.store;
            match store.type_ref(ty) {
                TypeRef::Path(path) => {
                    let hygiene = store.type_path_hygiene(ty);
                    match self.resolver.resolve_path_in_type_ns_hygienic(self.db, path, hygiene) {
                        Some((TypeNs::GenericParam(id), None, ..)) => self.type_or_const(id.into()),
                        Some((TypeNs::BuiltinType(_), None, ..)) => {
                            path.mod_path().map_or_else(String::new, |it| {
                                it.display_verbatim(self.db).to_string()
                            })
                        }
                        Some((other, ..)) => format!("{other:?}"),
                        None => "unresolved".to_owned(),
                    }
                }
                TypeRef::TypeParam(id) => self.type_or_const(id.into()),
                TypeRef::Tuple(tys) => {
                    let tys: Vec<_> = tys.iter().map(|&it| self.ty(it)).collect();
                    format!("({})", tys.join(", "))
                }
                TypeRef::Reference(it) => {
                    format!("&{} {}", self.lifetime(it.lifetime), self.ty(it.ty))
                }
                TypeRef::Array(it) => format!("[{}; {}]", self.ty(it.ty), self.konst(it.len.expr)),
                other => format!("{other:?}"),
            }
        }
        fn konst(&self, expr: crate::hir::ExprId) -> String {
            let store = &self.signature.store;
            let Expr::Path(path) = store.expr(expr) else { return "not a path".to_owned() };
            let hygiene = store.expr_path_hygiene(expr);
            match self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene) {
                Some(ValueNs::GenericParam(id)) => self.type_or_const(id.into()),
                Some(other) => format!("{other:?}"),
                None => "unresolved".to_owned(),
            }
        }
        fn bound(&self, bound: &TypeBound) -> String {
            match *bound {
                TypeBound::Path(path, _) => self
                    .signature
                    .store
                    .path(path)
                    .mod_path()
                    .map_or_else(String::new, |it| it.display_verbatim(self.db).to_string()),
                TypeBound::Lifetime(it) => self.lifetime(Some(it)),
                other => format!("{other:?}"),
            }
        }
    }

    let db = fixture::db(ra_fixture).into_db();
    let krate = db.fetch_test_crate();
    let mut functions = Vec::new();
    collect_functions(&db, crate_def_map(&db, krate), &mut functions);
    let mut out = String::new();
    for function in functions {
        let signature = FunctionSignature::of(&db, function);
        let cx = Cx { db: &db, resolver: function.resolver(&db), signature, def: function.into() };
        let params = cx.params();
        let mut list: Vec<String> =
            params.iter_lt().map(|(id, it)| label(&it.name, id.into_raw().into_u32())).collect();
        list.extend(params.iter_type_or_consts().map(|(id, it)| {
            let name = it.name().expect("a named parameter");
            let kind = if it.const_param().is_some() { "const " } else { "" };
            format!("{kind}{}", label(name, id.into_raw().into_u32()))
        }));
        let inputs: Vec<String> = signature.params.iter().map(|&it| cx.ty(it)).collect();
        let ret = signature.ret_type.map_or_else(|| "()".to_owned(), |it| cx.ty(it));
        writeln!(
            out,
            "{}: <{}>({}) -> {ret}",
            signature.name.as_str(),
            list.join(", "),
            inputs.join(", ")
        )
        .unwrap();
        for predicate in params.where_predicates() {
            let line = match predicate {
                WherePredicate::TypeBound { target, bound, .. } => {
                    format!("{}: {}", cx.ty(*target), cx.bound(bound))
                }
                WherePredicate::Lifetime { target, bound } => {
                    format!("{}: {}", cx.lifetime(Some(*target)), cx.lifetime(Some(*bound)))
                }
            };
            writeln!(out, "    {line}").unwrap();
        }
    }
    expect.assert_eq(&out);
}

#[test]
fn generic_parameters_have_macros_2_0_hygiene() {
    // rustc's tests/ui/hygiene/generic_params.rs (check-pass), its three modules: a type,
    // lifetime or const parameter an opaque `macro` writes is its own (`f`'s `T*` is not the
    // caller's `$T`), while a semi-opaque or transparent one's is the caller's (`g`'s `T` names
    // `$T`, `h`'s `$T` names `T`), whatever transparency locals have.
    check_generic_params(
        r#"
#![feature(decl_macro, rustc_attrs)]
mod type_params {
    macro m($T:ident) {
        fn f<$T: Clone, T: PartialEq>(t1: $T, t2: T) -> ($T, bool) {
            (t1.clone(), t2 == t2)
        }
    }

    #[rustc_macro_transparency = "semiopaque"]
    macro n($T:ident) {
        fn g<$T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
        fn h<T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
    }

    #[rustc_macro_transparency = "transparent"]
    macro p($T:ident) {
        fn j<$T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
        fn k<T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
    }

    m!(T);
    n!(T);
    p!(T);
}

mod lifetime_params {
    macro m($a:lifetime) {
        fn f<'b, 'c, $a: 'b, 'a: 'c>(t1: &$a(), t2: &'a ()) -> (&'b (), &'c ()) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "semiopaque"]
    macro n($a:lifetime) {
        fn g<$a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
        fn h<'a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "transparent"]
    macro p($a:lifetime) {
        fn j<$a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
        fn k<'a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
    }

    m!('a);
    n!('a);
    p!('a);
}

mod const_params {
    macro m($C:ident) {
        fn f<const $C: usize, const C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); $C], [(); C]) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "semiopaque"]
    macro n($C:ident) {
        fn g<const $C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
        fn h<const C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "transparent"]
    macro p($C:ident) {
        fn j<const $C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
        fn k<const C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
    }

    m!(C);
    n!(C);
    p!(C);
}

fn main() {}
"#,
        &expect![[r#"
            main: <>() -> ()
            f: <T#0, T*#1>(T#0, T*#1) -> (T#0, bool)
                T#0: Clone
                T*#1: PartialEq
            g: <T#0>(T#0, T#0) -> (T#0, T#0)
                T#0: Clone
            h: <T#0>(T#0, T#0) -> (T#0, T#0)
                T#0: Clone
            j: <T#0>(T#0, T#0) -> (T#0, T#0)
                T#0: Clone
            k: <T#0>(T#0, T#0) -> (T#0, T#0)
                T#0: Clone
            f: <'b*#0, 'c*#1, 'a#2, 'a*#3>(&'a#2 (), &'a*#3 ()) -> (&'b*#0 (), &'c*#1 ())
                'a#2: 'b*#0
                'a*#3: 'c*#1
            g: <'a#0>(&'a#0 (), &'a#0 ()) -> (&'a#0 (), &'a#0 ())
            h: <'a#0>(&'a#0 (), &'a#0 ()) -> (&'a#0 (), &'a#0 ())
            j: <'a#0>(&'a#0 (), &'a#0 ()) -> (&'a#0 (), &'a#0 ())
            k: <'a#0>(&'a#0 (), &'a#0 ()) -> (&'a#0 (), &'a#0 ())
            f: <const C#0, const C*#1>([(); C#0], [(); C*#1]) -> ([(); C#0], [(); C*#1])
            g: <const C#0>([(); C#0], [(); C#0]) -> ([(); C#0], [(); C#0])
            h: <const C#0>([(); C#0], [(); C#0]) -> ([(); C#0], [(); C#0])
            j: <const C#0>([(); C#0], [(); C#0]) -> ([(); C#0], [(); C#0])
            k: <const C#0>([(); C#0], [(); C#0]) -> ([(); C#0], [(); C#0])
        "#]],
    );
}

#[test]
fn a_parameter_a_macro_rules_writes_is_the_callers() {
    // Ordinary code is unchanged: `macro_rules!` and a real file key no parameter, so a
    // `macro_rules!`'s `T` names the caller's `$T`, as rustc's `macro_rules!` transparency
    // (semi-opaque) normalizes to the root for macros 2.0.
    check_generic_params(
        r#"
macro_rules! m {
    ($T:ident) => {
        fn f<$T>(t: T) -> $T { t }
    };
}
m!(T);
fn g<'a, T, const N: usize>(t: &'a [T; N]) -> &'a T { &t[0] }
"#,
        &expect![[r#"
            f: <T#0>(T#0) -> T#0
            g: <'a#0, T#0, const N#1>(&'a#0 [T#0; N#1]) -> &'a#0 T#0
        "#]],
    );
}

#[test]
fn a_crate_root_a_macro_writes_is_its_crates() {
    // rustc's `resolve_crate_root`: the `crate` of an import a `macro` of another crate writes,
    // and in 2015 its `::` and the `{{root}}` a plain import path gets, name the root of the
    // crate defining the `macro` (the last mark `adjust` removes), not the call site's. Each
    // `y` is `foo`'s `fn x`, not `main`'s module or `inner`'s unit struct; `e::z` is `main`'s
    // module `x`, though `d`, the module defining `m2`, is not around `e`. The pinned rustc
    // compiles this program.
    check_resolutions(
        r#"
//- /main.rs crate:main edition:2015 deps:foo
#![feature(decl_macro)]
pub mod x { pub fn here() {} }
foo::inner::m!(y1, y2, y3);
pub mod d {
    pub macro m2($g:ident) { pub use x as $g; }
}
pub mod e { crate::d::m2!(z); }
fn main() { y1(); y2(); y3(); e::z::here(); }

//- /lib.rs crate:foo edition:2015
#![feature(decl_macro)]
pub fn x() {}
pub mod inner {
    pub struct x;
    pub macro m($a:ident, $b:ident, $c:ident) {
        pub use crate::x as $a;
        pub use ::x as $b;
        pub use x as $c;
    }
}
"#,
        &expect![[r#"
            fn main -> ():
                y1 -> fn x -> ()
                y2 -> fn x -> ()
                y3 -> fn x -> ()
                e::z::here -> fn here -> ()
        "#]],
    );
}

#[test]
fn each_later_super_a_macro_writes_starts_from_its_own_context() {
    // rustc's `resolve_path` of each `super`: the first shares the context `resolve_self`
    // moved (here past the crate root, to `d`, the module defining `m`, with its mark removed),
    // and each later one starts again from its own keyword's context, so its `resolve_self`
    // from the crate root goes past it to `d` again. `g` is `d`'s struct `f`, not the root's
    // `fn f`. The pinned rustc compiles this program.
    check_resolutions(
        r#"
#![feature(decl_macro)]
pub mod a {
    pub mod d {
        pub struct f;
        pub macro m($g:ident) { pub use super::super::f as $g; }
        pub mod e { super::m!(g); }
    }
}
pub fn f() {}
fn main() { let _: a::d::f = a::d::e::g; }
"#,
        &expect![[r#"
            fn main -> ():
                a::d::e::g -> struct f
        "#]],
    );
}

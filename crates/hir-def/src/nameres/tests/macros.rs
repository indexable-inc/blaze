use base_db::{Db, all_crates};
use expect_test::expect;

use itertools::Itertools;
use span::Edition;

use super::*;

#[test]
fn macro_rules_are_globally_visible() {
    check(
        r#"
//- /lib.rs
macro_rules! structs {
    ($($i:ident),*) => {
        $(struct $i { field: u32 } )*
    }
}
structs!(Foo);
mod nested;

//- /nested.rs
structs!(Bar, Baz);
"#,
        expect![[r#"
            crate
            - Foo : type
            - nested : type
            - (legacy) structs : macro!

            crate::nested
            - Bar : type
            - Baz : type
            - (legacy) structs : macro!
        "#]],
    );
}

#[test]
fn macro_rules_can_define_modules() {
    check(
        r#"
//- /lib.rs
macro_rules! m {
    ($name:ident) => { mod $name;  }
}
m!(n1);
mod m { m!(n3) }

//- /n1.rs
m!(n2)
//- /n1/n2.rs
struct X;
//- /m/n3.rs
struct Y;
"#,
        expect![[r#"
            crate
            - m : type
            - n1 : type
            - (legacy) m : macro!

            crate::m
            - n3 : type
            - (legacy) m : macro!

            crate::m::n3
            - Y : type value
            - (legacy) m : macro!

            crate::n1
            - n2 : type
            - (legacy) m : macro!

            crate::n1::n2
            - X : type value
            - (legacy) m : macro!
        "#]],
    );
}

#[test]
fn macro_rules_from_other_crates_are_visible() {
    check(
        r#"
//- /main.rs crate:main deps:foo
foo::structs!(Foo, Bar)
mod bar;

//- /bar.rs
use crate::*;

//- /lib.rs crate:foo
#[macro_export]
macro_rules! structs {
    ($($i:ident),*) => {
        $(struct $i { field: u32 } )*
    }
}
"#,
        expect![[r#"
            crate
            - Bar : type
            - Foo : type
            - bar : type

            crate::bar
            - Bar : type (glob)
            - Foo : type (glob)
            - bar : type (glob)
        "#]],
    );
}

#[test]
fn macro_rules_defined_by_a_macro_of_another_crate_are_visible_in_child_modules() {
    check(
        r#"
//- /main.rs crate:main deps:foo
mod before {}
foo::define!(structs);
mod bar {
    structs!(Inner);
}
mod baz;

//- /baz.rs
structs!(InFile);

//- /lib.rs crate:foo
#[macro_export]
macro_rules! define {
    ($mac:ident) => {
        macro_rules! $mac {
            ($i:ident) => { pub struct $i; }
        }
    }
}
"#,
        expect![[r#"
            crate
            - bar : type
            - baz : type
            - before : type
            - (legacy) structs : macro!

            crate::bar
            - Inner : type value
            - (legacy) structs : macro!

            crate::baz
            - InFile : type value
            - (legacy) structs : macro!

            crate::before
        "#]],
    );
}

#[test]
fn macro_rules_export_with_local_inner_macros_are_visible() {
    check(
        r#"
//- /main.rs crate:main deps:foo
foo::structs!(Foo, Bar)
mod bar;

//- /bar.rs
use crate::*;

//- /lib.rs crate:foo
#[macro_export(local_inner_macros)]
macro_rules! structs {
    ($($i:ident),*) => {
        $(struct $i { field: u32 } )*
    }
}
"#,
        expect![[r#"
            crate
            - Bar : type
            - Foo : type
            - bar : type

            crate::bar
            - Bar : type (glob)
            - Foo : type (glob)
            - bar : type (glob)
        "#]],
    );
}

#[test]
fn local_inner_macros_makes_local_macros_usable() {
    check(
        r#"
//- /main.rs crate:main deps:foo
foo::structs!(Foo, Bar);
mod bar;

//- /bar.rs
use crate::*;

//- /lib.rs crate:foo
#[macro_export(local_inner_macros)]
macro_rules! structs {
    ($($i:ident),*) => {
        inner!($($i),*);
    }
}
#[macro_export]
macro_rules! inner {
    ($($i:ident),*) => {
        $(struct $i { field: u32 } )*
    }
}
"#,
        expect![[r#"
            crate
            - Bar : type
            - Foo : type
            - bar : type

            crate::bar
            - Bar : type (glob)
            - Foo : type (glob)
            - bar : type (glob)
        "#]],
    );
}

#[test]
fn unexpanded_macro_should_expand_by_fixedpoint_loop() {
    check(
        r#"
//- /main.rs crate:main deps:foo
macro_rules! baz {
    () => {
        use foo::bar;
    }
}
foo!();
bar!();
baz!();

//- /lib.rs crate:foo
#[macro_export]
macro_rules! foo {
    () => {
        struct Foo { field: u32 }
    }
}
#[macro_export]
macro_rules! bar {
    () => {
        use foo::foo;
    }
}
"#,
        expect![[r#"
            crate
            - Foo : type
            - bar : macro! (import)
            - foo : macro! (import)
            - (legacy) baz : macro!
        "#]],
    );
}

#[test]
fn macro_rules_from_other_crates_are_visible_with_macro_use() {
    cov_mark::check!(macro_rules_from_other_crates_are_visible_with_macro_use);
    check(
        r#"
//- /main.rs crate:main deps:foo
structs!(Foo);
structs_priv!(Bar);
structs_not_exported!(MacroNotResolved1);
crate::structs!(MacroNotResolved2);

mod bar;

#[macro_use]
extern crate foo;

//- /bar.rs
structs!(Baz);
crate::structs!(MacroNotResolved3);

//- /lib.rs crate:foo
#[macro_export]
macro_rules! structs {
    ($i:ident) => { struct $i; }
}

macro_rules! structs_not_exported {
    ($i:ident) => { struct $i; }
}

mod priv_mod {
    #[macro_export]
    macro_rules! structs_priv {
        ($i:ident) => { struct $i; }
    }
}
"#,
        expect![[r#"
            crate
            - Bar : type value
            - Foo : type value
            - bar : type
            - foo : type (extern)

            crate::bar
            - Baz : type value
        "#]],
    );
}

#[test]
fn macro_use_filter() {
    check(
        r#"
//- /main.rs crate:main deps:empty,multiple,all
#[macro_use()]
extern crate empty;

foo_not_imported!();

#[macro_use(bar1)]
#[macro_use()]
#[macro_use(bar2, bar3)]
extern crate multiple;

bar1!();
bar2!();
bar3!();
bar_not_imported!();

#[macro_use(baz1)]
#[macro_use]
#[macro_use(baz2)]
extern crate all;

baz1!();
baz2!();
baz3!();

//- /empty.rs crate:empty
#[macro_export]
macro_rules! foo_not_imported { () => { struct NotOkFoo; } }

//- /multiple.rs crate:multiple
#[macro_export]
macro_rules! bar1 { () => { struct OkBar1; } }
#[macro_export]
macro_rules! bar2 { () => { struct OkBar2; } }
#[macro_export]
macro_rules! bar3 { () => { struct OkBar3; } }
#[macro_export]
macro_rules! bar_not_imported { () => { struct NotOkBar; } }

//- /all.rs crate:all
#[macro_export]
macro_rules! baz1 { () => { struct OkBaz1; } }
#[macro_export]
macro_rules! baz2 { () => { struct OkBaz2; } }
#[macro_export]
macro_rules! baz3 { () => { struct OkBaz3; } }
"#,
        expect![[r#"
            crate
            - OkBar1 : type value
            - OkBar2 : type value
            - OkBar3 : type value
            - OkBaz1 : type value
            - OkBaz2 : type value
            - OkBaz3 : type value
            - all : type (extern)
            - empty : type (extern)
            - multiple : type (extern)
        "#]],
    );
}

#[test]
fn prelude_is_not_macro_use() {
    check(
        r#"
//- /main.rs edition:2018 crate:main deps:std
structs!(Foo);
structs_outside!(MacroNotResolved);

//- /lib.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub macro structs {
            ($i:ident) => { struct $i; }
        }
    }
}

#[macro_export]
macro_rules! structs_outside {
    ($i:ident) => { struct $i; }
}
"#,
        expect![[r#"
            crate
            - Foo : type value
        "#]],
    );
}

#[test]
fn prelude_cycle() {
    check(
        r#"
#[prelude_import]
use self::prelude::*;

declare_mod!();

mod prelude {
    macro_rules! declare_mod {
        () => (mod foo {})
    }
}
"#,
        expect![[r#"
            crate
            - prelude : type

            crate::prelude
            - (legacy) declare_mod : macro!
        "#]],
    );
}

#[test]
fn legacy_macro_use_before_def() {
    check(
        r#"
m!();

macro_rules! m {
    () => {
        struct S;
    }
}
"#,
        expect![[r#"
            crate
            - S : type value
            - (legacy) m : macro!
        "#]],
    );
    // FIXME: should not expand. legacy macro scoping is not implemented.
}

#[test]
fn plain_macros_are_legacy_textual_scoped() {
    check(
        r#"
//- /main.rs
mod m1;
bar!(NotFoundNotMacroUse);

mod m2 { foo!(NotFoundBeforeInside2); }

macro_rules! foo {
    ($x:ident) => { struct $x; }
}
foo!(Ok);

mod m3;
foo!(OkShadowStop);
bar!(NotFoundMacroUseStop);

#[macro_use]
mod m5 {
    #[macro_use]
    mod m6 {
        macro_rules! foo {
            ($x:ident) => { fn $x() {} }
        }
    }
}
foo!(ok_double_macro_use_shadow);

baz!(NotFoundBefore);
#[macro_use]
mod m7 {
    macro_rules! baz {
        ($x:ident) => { struct $x; }
    }
}
baz!(OkAfter);

//- /m1.rs
foo!(NotFoundBeforeInside1);
macro_rules! bar {
    ($x:ident) => { struct $x; }
}

//- /m3/mod.rs
foo!(OkAfterInside);
macro_rules! foo {
    ($x:ident) => { fn $x() {} }
}
foo!(ok_shadow);

#[macro_use]
mod m4;
bar!(OkMacroUse);

mod m5;
baz!(OkMacroUseInner);

//- /m3/m4.rs
foo!(ok_shadow_deep);
macro_rules! bar {
    ($x:ident) => { struct $x; }
}
//- /m3/m5.rs
#![macro_use]
macro_rules! baz {
    ($x:ident) => { struct $x; }
}


"#,
        expect![[r#"
            crate
            - NotFoundBefore : type value
            - Ok : type value
            - OkAfter : type value
            - OkShadowStop : type value
            - m1 : type
            - m2 : type
            - m3 : type
            - m5 : type
            - m7 : type
            - ok_double_macro_use_shadow : value
            - (legacy) baz : macro!
            - (legacy) foo : macro! macro! macro!

            crate::m1
            - (legacy) bar : macro!

            crate::m2

            crate::m3
            - OkAfterInside : type value
            - OkMacroUse : type value
            - OkMacroUseInner : type value
            - m4 : type
            - m5 : type
            - ok_shadow : value
            - (legacy) bar : macro! macro!
            - (legacy) baz : macro!
            - (legacy) foo : macro! macro! macro! macro!

            crate::m3::m4
            - ok_shadow_deep : value
            - (legacy) bar : macro!
            - (legacy) foo : macro! macro!

            crate::m3::m5
            - (legacy) bar : macro!
            - (legacy) baz : macro!
            - (legacy) foo : macro! macro! macro!

            crate::m5
            - m6 : type
            - (legacy) foo : macro! macro!

            crate::m5::m6
            - (legacy) foo : macro! macro!

            crate::m7
            - (legacy) baz : macro!
            - (legacy) foo : macro! macro!
        "#]],
    );
    // FIXME: should not see `NotFoundBefore`
}

#[test]
fn type_value_macro_live_in_different_scopes() {
    check(
        r#"
#[macro_export]
macro_rules! foo {
    ($x:ident) => { type $x = (); }
}

foo!(foo);
use foo as bar;

use self::foo as baz;
fn baz() {}
"#,
        expect![[r#"
            crate
            - bar : type (import) macro! (import)
            - baz : type (import) value macro! (import)
            - foo : type macro!
            - (legacy) foo : macro!
        "#]],
    );
}

#[test]
fn macro_use_can_be_aliased() {
    check(
        r#"
//- /main.rs crate:main deps:foo
#[macro_use]
extern crate foo;

foo!(Direct);
bar!(Alias);

//- /lib.rs crate:foo
use crate::foo as bar;

mod m {
    #[macro_export]
    macro_rules! foo {
        ($x:ident) => { struct $x; }
    }
}
"#,
        expect![[r#"
            crate
            - Alias : type value
            - Direct : type value
            - foo : type (extern)
        "#]],
    );
}

#[test]
fn path_qualified_macros() {
    check(
        r#"
macro_rules! foo {
    ($x:ident) => { struct $x; }
}

crate::foo!(NotResolved);

crate::bar!(OkCrate);
bar!(OkPlain);
alias1!(NotHere);
m::alias1!(OkAliasPlain);
m::alias2!(OkAliasSuper);
m::alias3!(OkAliasCrate);
not_found!(NotFound);

mod m {
    #[macro_export]
    macro_rules! bar {
        ($x:ident) => { struct $x; }
    }
    pub use bar as alias1;
    pub use super::bar as alias2;
    pub use crate::bar as alias3;
    pub use self::bar as not_found;
}
"#,
        expect![[r#"
            crate
            - OkAliasCrate : type value
            - OkAliasPlain : type value
            - OkAliasSuper : type value
            - OkCrate : type value
            - OkPlain : type value
            - bar : macro!
            - m : type
            - (legacy) foo : macro!

            crate::m
            - alias1 : macro! (import)
            - alias2 : macro! (import)
            - alias3 : macro! (import)
            - not_found : _
            - (legacy) bar : macro!
            - (legacy) foo : macro!
        "#]],
    );
}

#[test]
fn macro_dollar_crate_is_correct_in_item() {
    cov_mark::check!(macro_dollar_crate_self);
    check(
        r#"
//- /main.rs crate:main deps:foo
#[macro_use]
extern crate foo;

#[macro_use]
mod m {
    macro_rules! current {
        () => {
            use $crate::Foo as FooSelf;
        }
    }
}

struct Foo;

current!();
not_current1!();
foo::not_current2!();

//- /lib.rs crate:foo
mod m {
    #[macro_export]
    macro_rules! not_current1 {
        () => {
            use $crate::Bar;
        }
    }
}

#[macro_export]
macro_rules! not_current2 {
    () => {
        use $crate::Baz;
    }
}

pub struct Bar;
pub struct Baz;
"#,
        expect![[r#"
            crate
            - Bar : type (import) value (import)
            - Baz : type (import) value (import)
            - Foo : type value
            - FooSelf : type (import) value (import)
            - foo : type (extern)
            - m : type
            - (legacy) current : macro!

            crate::m
            - (legacy) current : macro!
        "#]],
    );
}

#[test]
fn macro_dollar_crate_is_correct_in_indirect_deps() {
    cov_mark::check!(macro_dollar_crate_other);
    // From std
    check(
        r#"
//- /main.rs edition:2018 crate:main deps:std
foo!();

//- /std.rs crate:std deps:core
pub use core::foo;

pub mod prelude {
    pub mod rust_2018 {
        pub use crate::foo;
    }
}

//- /core.rs crate:core
#[macro_export]
macro_rules! foo {
    () => {
        use $crate::bar;
    }
}

pub struct bar;
"#,
        expect![[r#"
            crate
            - bar : type (import) value (import)
        "#]],
    );
}

#[test]
fn macro_dollar_crate_is_correct_in_derive_meta() {
    compute_crate_def_map(
        r#"
//- minicore: derive, clone
//- /main.rs crate:main deps:lib
lib::foo!();

//- /lib.rs crate:lib
#[macro_export]
macro_rules! foo {
    () => {
        #[derive($crate::Clone)]
        struct S;
    }
}

pub use core::clone::Clone;
"#,
        |db, map| assert_eq!(map.module(db, map.root).scope(db).builtin_derive_impls().len(), 1),
    );
}

#[test]
fn expand_derive() {
    compute_crate_def_map(
        r#"
//- /main.rs crate:main deps:core
use core::Copy;

#[core::derive(Copy, core::Clone)]
struct Foo;

//- /core.rs crate:core
#[rustc_builtin_macro]
pub macro derive($item:item) {}
#[rustc_builtin_macro]
pub macro Copy {}
#[rustc_builtin_macro]
pub macro Clone {}
"#,
        |db, map| assert_eq!(map.module(db, map.root).scope(db).builtin_derive_impls().len(), 2),
    );
}

#[test]
fn resolve_builtin_derive() {
    check(
        r#"
//- /main.rs crate:main deps:core
use core::*;

//- /core.rs crate:core
#[rustc_builtin_macro]
pub macro Clone {}

pub trait Clone {}
"#,
        expect![[r#"
            crate
            - Clone : type (glob) macro# (glob)
        "#]],
    );
}

#[test]
fn builtin_derive_with_unresolved_attributes_fall_back() {
    // Tests that we still resolve derives after ignoring an unresolved attribute.
    cov_mark::check!(unresolved_attribute_fallback);
    compute_crate_def_map(
        r#"
//- /main.rs crate:main deps:core
use core::{Clone, derive};

#[derive(Clone)]
#[unresolved]
struct Foo;

//- /core.rs crate:core
#[rustc_builtin_macro]
pub macro derive($item:item) {}
#[rustc_builtin_macro]
pub macro Clone {}
"#,
        |db, map| assert_eq!(map.module(db, map.root).scope(db).builtin_derive_impls().len(), 1),
    );
}

#[test]
fn unresolved_attributes_fall_back_track_per_file_moditems() {
    // Tests that we track per-file ModItems when ignoring an unresolved attribute.
    // Just tracking the `ModItem` leads to `Foo` getting ignored.

    check(
        r#"
        //- /main.rs crate:main

        mod submod;

        #[unresolved]
        struct Foo;

        //- /submod.rs
        #[unresolved]
        struct Bar;
        "#,
        expect![[r#"
            crate
            - Foo : type value
            - submod : type

            crate::submod
            - Bar : type value
        "#]],
    );
}

#[test]
fn unresolved_attrs_extern_block_hang() {
    // Regression test for https://github.com/rust-lang/rust-analyzer/issues/8905
    check(
        r#"
#[unresolved]
extern "C" {
    #[unresolved]
    fn f();
}
    "#,
        expect![[r#"
            crate
            - f : value
        "#]],
    );
}

#[test]
fn macros_in_extern_block() {
    check(
        r#"
macro_rules! m {
    () => { static S: u8; };
}

extern {
    m!();
}
    "#,
        expect![[r#"
            crate
            - S : value
            - (legacy) m : macro!
        "#]],
    );
}

#[test]
fn resolves_derive_helper() {
    cov_mark::check!(resolved_derive_helper);
    check(
        r#"
//- /main.rs crate:main deps:proc
#[rustc_builtin_macro]
pub macro derive($item:item) {}

#[derive(proc::Derive)]
#[helper]
#[unresolved]
struct S;

//- /proc.rs crate:proc
#![crate_type="proc-macro"]
#[proc_macro_derive(Derive, attributes(helper))]
fn derive() {}
        "#,
        expect![[r#"
            crate
            - S : type value
            - derive : macro#
        "#]],
    );
}

#[test]
fn resolves_derive_helper_rustc_builtin_macro() {
    cov_mark::check!(resolved_derive_helper);
    // This is NOT the correct usage of `default` helper attribute, but we don't resolve helper
    // attributes on non mod items in hir nameres.
    check(
        r#"
//- minicore: derive, default
#[derive(Default)]
#[default]
enum E {
    A,
    B,
}
"#,
        expect![[r#"
            crate
            - E : type
        "#]],
    );
}

#[test]
fn unresolved_attr_with_cfg_attr_hang() {
    // Another regression test for https://github.com/rust-lang/rust-analyzer/issues/8905
    check(
        r#"
#[cfg_attr(not(off), unresolved, unresolved)]
struct S;
        "#,
        expect![[r#"
            crate
            - S : type value
        "#]],
    );
}

#[test]
fn macro_expansion_overflow() {
    cov_mark::check!(macro_expansion_overflow);
    check(
        r#"
macro_rules! a {
    ($e:expr; $($t:tt)*) => {
        b!(static = (); $($t)*);
    };
    () => {};
}

macro_rules! b {
    (static = $e:expr; $($t:tt)*) => {
        a!($e; $($t)*);
    };
    () => {};
}

b! { static = #[] ();}
"#,
        expect![[r#"
            crate
            - (legacy) a : macro!
            - (legacy) b : macro!
        "#]],
    );
}

/// An item-position call past `recursion_limit` is reported (rustc's "recursion limit reached
/// while expanding"), never dropped in silence.
#[test]
fn macro_expansion_overflow_is_reported() {
    compute_crate_def_map(
        r#"
#![recursion_limit = "8"]
macro_rules! m { () => { m!(); }; }
m!();
"#,
        |db, def_map| {
            let overflows: Vec<String> = (def_map.diagnostics(db).iter())
                .filter_map(|it| match &it.kind {
                    crate::nameres::diagnostics::DefDiagnosticKind::MacroError {
                        path,
                        err: hir_expand::ExpandErrorKind::RecursionOverflow,
                        ..
                    } => Some(path.display(db, Edition::CURRENT).to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(overflows, ["m"]);
        },
    );
}

#[test]
fn macros_defining_macros() {
    check(
        r#"
macro_rules! item {
    ($item:item) => { $item }
}

item! {
    macro_rules! indirect_macro { () => { struct S {} } }
}

indirect_macro!();
    "#,
        expect![[r#"
            crate
            - S : type
            - (legacy) indirect_macro : macro!
            - (legacy) item : macro!
        "#]],
    );
}

#[test]
fn resolves_proc_macros() {
    check(
        r#"
#![crate_type="proc-macro"]
struct TokenStream;

#[proc_macro]
pub fn function_like_macro(args: TokenStream) -> TokenStream {
    args
}

#[proc_macro_attribute]
pub fn attribute_macro(_args: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_derive(DummyTrait)]
pub fn derive_macro(_item: TokenStream) -> TokenStream {
    TokenStream
}

#[proc_macro_derive(AnotherTrait, attributes(helper_attr))]
pub fn derive_macro_2(_item: TokenStream) -> TokenStream {
    TokenStream
}
"#,
        expect![[r#"
            crate
            - AnotherTrait : macro#
            - DummyTrait : macro#
            - TokenStream : type value
            - attribute_macro : value macro#
            - derive_macro : value
            - derive_macro_2 : value
            - function_like_macro : value macro!
        "#]],
    );
}

#[test]
fn proc_macro_censoring() {
    // Make sure that only proc macros are publicly exported from proc-macro crates.

    check(
        r#"
//- /main.rs crate:main deps:macros
pub use macros::*;

//- /macros.rs crate:macros
#![crate_type="proc-macro"]
pub struct TokenStream;

#[proc_macro]
pub fn function_like_macro(args: TokenStream) -> TokenStream {
    args
}

#[proc_macro_attribute]
pub fn attribute_macro(_args: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_derive(DummyTrait)]
pub fn derive_macro(_item: TokenStream) -> TokenStream {
    TokenStream
}

#[macro_export]
macro_rules! mbe {
    () => {};
}
"#,
        expect![[r#"
            crate
            - DummyTrait : macro# (glob)
            - attribute_macro : macro# (glob)
            - function_like_macro : macro! (glob)
        "#]],
    );
}

#[test]
fn collects_derive_helpers() {
    let db = fixture::db(
        r#"
#![crate_type="proc-macro"]
struct TokenStream;

#[proc_macro_derive(AnotherTrait, attributes(helper_attr))]
pub fn derive_macro_2(_item: TokenStream) -> TokenStream {
    TokenStream
}
"#,
    )
    .into_db();
    let krate = *all_crates(&db).last().expect("no crate graph present");
    let def_map = crate_def_map(&db, krate);

    assert_eq!(def_map.data.exported_derives.len(), 1);
    match def_map.data.exported_derives.values().next() {
        Some(helpers) => match &**helpers {
            [attr] => assert_eq!(attr.display(&db, Edition::CURRENT).to_string(), "helper_attr"),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

#[test]
fn resolve_macro_def() {
    check(
        r#"
pub macro structs($($i:ident),*) {
    $(struct $i { field: u32 } )*
}
structs!(Foo);
"#,
        expect![[r#"
            crate
            - Foo : type
            - structs : macro!
        "#]],
    );
}

#[test]
fn macro_in_prelude() {
    check(
        r#"
//- /lib.rs edition:2018 crate:lib deps:std
global_asm!();

//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub macro global_asm() {
            pub struct S;
        }
    }
}
        "#,
        expect![[r#"
            crate
            - S : type value
        "#]],
    )
}

#[test]
fn issue9358_bad_macro_stack_overflow() {
    cov_mark::check!(issue9358_bad_macro_stack_overflow);
    check(
        r#"
macro_rules! m {
  ($cond:expr) => { m!($cond, stringify!($cond)) };
  ($cond:expr, $($arg:tt)*) => { $cond };
}
m!(
"#,
        expect![[r#"
            crate
            - (legacy) m : macro!
        "#]],
    )
}

#[test]
fn eager_macro_correctly_resolves_contents() {
    // Eager macros resolve any contained macros when expanded. This should work correctly with the
    // usual name resolution rules, so both of these `include!`s should include the right file.

    check(
        r#"
//- /lib.rs
#[rustc_builtin_macro]
macro_rules! include { () => {} }

include!(inner_a!());
include!(crate::inner_b!());

#[macro_export]
macro_rules! inner_a {
    () => { "inc_a.rs" };
}
#[macro_export]
macro_rules! inner_b {
    () => { "inc_b.rs" };
}
//- /inc_a.rs
struct A;
//- /inc_b.rs
struct B;
"#,
        expect![[r#"
            crate
            - A : type value
            - B : type value
            - inner_a : macro!
            - inner_b : macro!
            - (legacy) include : macro!
            - (legacy) inner_a : macro!
            - (legacy) inner_b : macro!
        "#]],
    );
}

#[test]
fn eager_macro_correctly_resolves_dollar_crate() {
    // MBE -> eager -> $crate::mbe
    check(
        r#"
//- /lib.rs
#[rustc_builtin_macro]
macro_rules! include { () => {} }

#[macro_export]
macro_rules! inner {
    () => { "inc.rs" };
}

macro_rules! m {
    () => { include!($crate::inner!()); };
}

m!();

//- /inc.rs
struct A;
"#,
        expect![[r#"
            crate
            - A : type value
            - inner : macro!
            - (legacy) include : macro!
            - (legacy) inner : macro!
            - (legacy) m : macro!
        "#]],
    );
    // eager -> MBE -> $crate::mbe
    check(
        r#"
//- /lib.rs
#[rustc_builtin_macro]
macro_rules! include { () => {} }

#[macro_export]
macro_rules! inner {
    () => { "inc.rs" };
}

macro_rules! n {
    () => {
        $crate::inner!()
    };
}

include!(n!());

//- /inc.rs
struct A;
"#,
        expect![[r#"
            crate
            - A : type value
            - inner : macro!
            - (legacy) include : macro!
            - (legacy) inner : macro!
            - (legacy) n : macro!
        "#]],
    );
}

#[test]
fn nested_include() {
    check(
        r#"
//- minicore: include
//- /lib.rs
include!("out_dir/includes.rs");

//- /out_dir/includes.rs
pub mod company_name {
    pub mod network {
        pub mod v1 {
            include!("company_name.network.v1.rs");
        }
    }
}
//- /out_dir/company_name.network.v1.rs
pub struct IpAddress {
    pub ip_type: &'static str,
}
/// Nested message and enum types in `IpAddress`.
pub mod ip_address {
    pub enum IpType {
        IpV4(u32),
    }
}

"#,
        expect![[r#"
            crate
            - company_name : type

            crate::company_name
            - network : type

            crate::company_name::network
            - v1 : type

            crate::company_name::network::v1
            - IpAddress : type
            - ip_address : type

            crate::company_name::network::v1::ip_address
            - IpType : type
        "#]],
    );
}

#[test]
fn eager_include_names_a_macro_an_earlier_include_defined() {
    // rquickjs-sys: `include!` and `concat!` come from the prelude, so these calls resolve late,
    // and `bindings_env!` is a `macro_rules!` of the textual scope only.
    check(
        r#"
//- minicore: include, concat, env
//- /lib.rs env:OUT_DIR=out
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
include!(concat!("bindings/", bindings_env!("TARGET"), ".rs"));
pub struct After;
//- /out/bindings.rs
macro_rules! bindings_env { ("TARGET") => { "x86_64" }; }
pub struct FromOut;
//- /bindings/x86_64.rs
pub struct JSValue;
"#,
        expect![[r#"
            crate
            - After : type value
            - FromOut : type value
            - JSValue : type value
            - (legacy) bindings_env : macro!
        "#]],
    );
}

#[test]
fn macro_use_module_exports_macro_rules_an_include_defined() {
    // syn 1.0.109: `#[macro_use] mod token` gets `Token!` from `include!("await.rs")`, and
    // `mod op`, which imports nothing, names it.
    check(
        r#"
//- minicore: include
//- /lib.rs
#[macro_use]
mod token;
mod op;
//- /token.rs
macro_rules! export_token_macro {
    ($($await_rule:tt)*) => {
        #[macro_export]
        macro_rules! Token {
            [+] => { pub struct Add; };
        }
    };
}
include!("await.rs");
//- /await.rs
export_token_macro! {[await]}
//- /op.rs
Token![+];
"#,
        expect![[r#"
            crate
            - Token : macro!
            - op : type
            - token : type
            - (legacy) Token : macro!
            - (legacy) export_token_macro : macro!

            crate::op
            - Add : type value
            - (legacy) Token : macro!
            - (legacy) export_token_macro : macro!

            crate::token
            - (legacy) Token : macro!
            - (legacy) export_token_macro : macro!
        "#]],
    );
}

#[test]
fn include_with_submod_file() {
    check(
        r#"
//- minicore: include
//- /lib.rs
include!("out_dir/includes.rs");

//- /out_dir/includes.rs
pub mod company_name {
    pub mod network {
        pub mod v1;
    }
}
//- /out_dir/company_name/network/v1.rs
pub struct IpAddress {
    pub ip_type: &'static str,
}
/// Nested message and enum types in `IpAddress`.
pub mod ip_address {
    pub enum IpType {
        IpV4(u32),
    }
}

"#,
        expect![[r#"
            crate
            - company_name : type

            crate::company_name
            - network : type

            crate::company_name::network
            - v1 : type

            crate::company_name::network::v1
            - IpAddress : type
            - ip_address : type

            crate::company_name::network::v1::ip_address
            - IpType : type
        "#]],
    );
}

#[test]
fn include_many_mods() {
    check(
        r#"
//- /lib.rs
#[rustc_builtin_macro]
macro_rules! include { () => {} }

mod nested {
    include!("out_dir/includes.rs");

    mod different_company {
        include!("out_dir/different_company/mod.rs");
    }

    mod util;
}

//- /nested/util.rs
pub struct Helper {}
//- /out_dir/includes.rs
pub mod company_name {
    pub mod network {
        pub mod v1;
    }
}
//- /out_dir/company_name/network/v1.rs
pub struct IpAddress {}
//- /out_dir/different_company/mod.rs
pub mod network;
//- /out_dir/different_company/network.rs
pub struct Url {}

"#,
        expect![[r#"
            crate
            - nested : type
            - (legacy) include : macro!

            crate::nested
            - company_name : type
            - different_company : type
            - util : type
            - (legacy) include : macro!

            crate::nested::company_name
            - network : type
            - (legacy) include : macro!

            crate::nested::company_name::network
            - v1 : type
            - (legacy) include : macro!

            crate::nested::company_name::network::v1
            - IpAddress : type
            - (legacy) include : macro!

            crate::nested::different_company
            - network : type
            - (legacy) include : macro!

            crate::nested::different_company::network
            - Url : type
            - (legacy) include : macro!

            crate::nested::util
            - Helper : type
            - (legacy) include : macro!
        "#]],
    );
}

#[test]
fn macro_use_imports_all_macro_types() {
    let db = fixture::db(
        r#"
//- /main.rs crate:main deps:lib
#[macro_use]
extern crate lib;

//- /lib.rs crate:lib deps:proc
pub use proc::*;

#[macro_export]
macro_rules! legacy { () => () }

pub macro macro20 {}

//- /proc.rs crate:proc
#![crate_type="proc-macro"]

struct TokenStream;

#[proc_macro_attribute]
fn proc_attr(a: TokenStream, b: TokenStream) -> TokenStream { a }
    "#,
    )
    .into_db();
    let krate = *all_crates(&db).last().expect("no crate graph present");
    let def_map = crate_def_map(&db, krate);

    let root_module = &def_map.module(&db, def_map.root).scope(&db);
    assert!(
        root_module.legacy_macros().count() == 0,
        "`#[macro_use]` shouldn't bring macros into textual macro scope",
    );

    let actual = def_map
        .macro_use_prelude
        .keys()
        .map(|name| name.display(&db, Edition::CURRENT).to_string())
        .sorted()
        .join("\n");

    expect![[r#"
        legacy
        macro20
        proc_attr"#]]
    .assert_eq(&actual);
}

#[test]
fn non_prelude_macros_take_precedence_over_macro_use_prelude() {
    check(
        r#"
//- /lib.rs edition:2021 crate:lib deps:dep,core
#[macro_use]
extern crate dep;

macro foo() { struct Ok; }
macro bar() { fn ok() {} }

foo!();
bar!();

//- /dep.rs crate:dep
#[macro_export]
macro_rules! foo {
    () => { struct NotOk; }
}

//- /core.rs crate:core
pub mod prelude {
    pub mod rust_2021 {
        #[macro_export]
        macro_rules! bar {
            () => { fn not_ok() {} }
        }
    }
}
        "#,
        expect![[r#"
            crate
            - Ok : type value
            - bar : macro!
            - dep : type (extern)
            - foo : macro!
            - ok : value
        "#]],
    );
}

#[test]
fn macro_use_prelude_is_eagerly_expanded() {
    // See FIXME in `ModCollector::collect_macro_call()`.
    check(
        r#"
//- /main.rs crate:main deps:lib
#[macro_use]
extern crate lib;
mk_foo!();
mod a {
    foo!();
}
//- /lib.rs crate:lib
#[macro_export]
macro_rules! mk_foo {
    () => {
        macro_rules! foo {
            () => { struct Ok; }
        }
    }
}
    "#,
        expect![[r#"
            crate
            - a : type
            - lib : type (extern)
            - (legacy) foo : macro!

            crate::a
            - Ok : type value
            - (legacy) foo : macro!
        "#]],
    );
}

#[test]
fn macro_sub_namespace() {
    compute_crate_def_map(
        r#"
//- minicore: derive, clone
macro_rules! Clone { () => {} }
macro_rules! derive { () => {} }

#[derive(Clone)]
struct S;
    "#,
        |db, map| assert_eq!(map.module(db, map.root).scope(db).builtin_derive_impls().len(), 1),
    );
}

#[test]
fn macro_sub_namespace2() {
    check(
        r#"
//- /main.rs edition:2021 crate:main deps:proc,core
use proc::{foo, bar};

foo!();
bar!();

//- /proc.rs crate:proc
#![crate_type="proc-macro"]
#[proc_macro_derive(foo)]
pub fn foo() {}
#[proc_macro_attribute]
pub fn bar() {}

//- /core.rs crate:core
pub mod prelude {
    pub mod rust_2021 {
        pub macro foo() {
            struct Ok;
        }
        pub macro bar() {
            fn ok() {}
        }
    }
}
    "#,
        expect![[r#"
            crate
            - Ok : type value
            - bar : macro# (import)
            - foo : macro# (import)
            - ok : value
        "#]],
    );
}

#[test]
fn macro_rules_mixed_style() {
    check(
        r#"

macro_rules! foo {
             () => {};
      attr() () => {};
    derive() () => {};
}

use foo;
"#,
        expect![[r#"
    crate
    - foo : macro!# (import)
    - (legacy) foo : macro!#
"#]],
    );
}

#[test]
fn macro_2_mixed_style() {
    check(
        r#"

macro foo {
             () => {};
      attr() () => {};
    derive() () => {};
}

use foo;
"#,
        expect![[r#"
            crate
            - foo : macro!#
        "#]],
    );
}

#[test]
fn macro_rules_attr() {
    check(
        r#"

macro_rules! my_attr {
    attr() ($($tt:tt)*) => { fn attr_fn() {} }
}

#[my_attr]
enum MyEnum {}

"#,
        expect![[r#"
    crate
    - attr_fn : value
    - (legacy) my_attr : macro#
"#]],
    );
}

#[test]
fn macro_2_attr() {
    check(
        r#"

macro my_attr {
    attr() ($($tt:tt)*) => { fn attr_fn() {} }
}

#[my_attr]
enum MyEnum {}

"#,
        expect![[r#"
    crate
    - attr_fn : value
    - my_attr : macro#
"#]],
    );
}

#[test]
fn macro_rules_derive() {
    check(
        r#"
//- minicore: derive

macro_rules! MyDerive {
    derive() ($($tt:tt)*) => { fn derived_fn() {} }
}

#[derive(MyDerive)]
enum MyEnum {}

"#,
        expect![[r#"
            crate
            - MyEnum : type
            - derived_fn : value
            - (legacy) MyDerive : macro#
        "#]],
    );
}

#[test]
fn macro_2_derive() {
    check(
        r#"
//- minicore: derive

macro MyDerive {
    derive() ($($tt:tt)*) => { fn derived_fn() {} }
}

#[derive(MyDerive)]
enum MyEnum {}

"#,
        expect![[r#"
            crate
            - MyDerive : macro#
            - MyEnum : type
            - derived_fn : value
        "#]],
    );
}

#[test]
fn regression_22806() {
    compute_crate_def_map(
        r#"
#![crate_type = "proc-macro"]

fn foo() {}

#[proc_macro]
fn foo() {}
    "#,
        |_, _| (),
    )
}

/// Writes `pub struct Big;` after more tokens than rust-analyzer used to keep of one
/// expansion (2,097,152), as biome_wasm's `#[wasm_bindgen]` block of 2271 types does.
#[derive(Debug)]
struct HugeExpander;
impl hir_expand::proc_macro::ProcMacroExpander for HugeExpander {
    fn expand(
        &self,
        _: &Db,
        hir_expand::proc_macro::ProcMacroCall { call_site, .. }: hir_expand::proc_macro::ProcMacroCall<'_>,
    ) -> Result<crate::tt::TopSubtree, hir_expand::proc_macro::ProcMacroExpansionError> {
        let text = format!(
            "macro_rules! ignore {{ ($($t:tt)*) => {{}}; }} ignore! {{ {} }} pub struct Big;",
            "0 ".repeat(2_200_000)
        );
        Ok(syntax_bridge::parse_to_token_tree_static_span(Edition::CURRENT, call_site, &text)
            .unwrap())
    }

    // dyn: `ProcMacroExpander::eq_dyn`'s signature, compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn hir_expand::proc_macro::ProcMacroExpander) -> bool {
        other.type_id() == std::any::TypeId::of::<Self>()
    }
}

#[test]
fn an_expansion_has_no_token_limit() {
    let proc_macros = vec![(
        r#"
#[proc_macro_attribute]
pub fn huge(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    TokenStream::new()
}
"#
        .into(),
        hir_expand::proc_macro::ProcMacro {
            name: intern::Symbol::intern("huge"),
            kind: hir_expand::proc_macro::ProcMacroKind::Attr,
            expander: std::sync::Arc::new(HugeExpander),
            disabled: false,
        },
    )];
    let db = fixture::db_with_proc_macros(
        r#"
//- proc_macros: huge
//- /main.rs crate:main
#[proc_macros::huge]
extern "C" {}
"#,
        proc_macros,
    )
    .into_db();
    let krate = *all_crates(&db)
        .iter()
        .find(|krate| {
            krate.extra_data(&db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                == Some("main")
        })
        .unwrap();
    let def_map = crate_def_map(&db, krate);
    let scope = def_map.module(&db, def_map.root).scope(&db);
    assert!(scope.entries().any(|(name, _)| name.as_str() == "Big"));
}

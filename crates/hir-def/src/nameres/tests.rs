mod backdate;
mod globs;
mod imports;
mod incremental;
mod macros;
mod mod_resolution;
mod paths;
mod prelude;
mod primitive_shadow;
mod stale_ids;
mod trait_imports;
mod uses;
mod worklist;
mod batch_order;
mod views;

use expect_test::{Expect, expect};
use hir_expand::fixture;

use crate::{
    nameres::{DefMap, crate_def_map},
    test_db::{TestDB, TestDBExt},
};

fn compute_crate_def_map(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    cb: impl FnOnce(&TestDB, &DefMap),
) {
    let db = fixture::db(ra_fixture).into_db();
    let krate = db.fetch_test_crate();
    cb(&db, crate_def_map(&db, krate));
}

fn render_crate_def_map(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> String {
    let db = fixture::db(ra_fixture).into_db();
    let krate = db.fetch_test_crate();
    crate_def_map(&db, krate).dump(&db)
}

fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let actual = render_crate_def_map(ra_fixture);
    expect.assert_eq(&actual);
}

#[test]
fn crate_def_map_smoke_test() {
    check(
        r#"
//- /lib.rs
mod foo;
struct S;
use crate::foo::bar::E;
use self::E::V;

//- /foo/mod.rs
pub mod bar;
fn f() {}

//- /foo/bar.rs
pub struct Baz;

union U { to_be: bool, not_to_be: u8 }
enum E { V }

extern {
    type Ext;
    static EXT: u8;
    fn ext();
}
"#,
        expect![[r#"
            crate
            - E : _
            - S : type value
            - V : _
            - foo : type

            crate::foo
            - bar : type
            - f : value

            crate::foo::bar
            - Baz : type value
            - E : type
            - EXT : value
            - Ext : type
            - U : type
            - ext : value
        "#]],
    );
}

#[test]
fn blaze_reassociate_is_an_inert_attribute_and_blaze_no_tool() {
    use hir_expand::{
        mod_path::{ModPath, PathKind},
        name::Name,
    };

    compute_crate_def_map("//- /lib.rs\n#[blaze::reassociate]\nfn f() {}\n", |db, def_map| {
        let path = |segments: &[&str]| {
            ModPath::from_segments(PathKind::Plain, segments.iter().map(|it| Name::new_root(it)))
        };
        assert!(def_map.is_builtin_or_registered_attr(&path(&["blaze", "reassociate"])));
        assert!(!def_map.is_builtin_or_registered_attr(&path(&["blaze", "cache"])));
        assert!(!def_map.is_builtin_or_registered_attr(&path(&["blaze"])));
        assert!(!def_map.is_builtin_or_registered_attr(&path(&["other", "reassociate"])));
        assert!(def_map.diagnostics(db).is_empty(), "{:?}", def_map.diagnostics(db));
    });
}

/// rustc's `builtin_attrs.rs` at the pinned nightly lists `sanitize` (feature `sanitize`) and
/// not `no_sanitize`, which it removed in 1.91.
#[test]
fn sanitize_is_a_builtin_attribute_and_no_sanitize_is_not() {
    use hir_expand::{
        mod_path::{ModPath, PathKind},
        name::Name,
    };

    let fixture = r#"
//- /lib.rs
#![feature(sanitize)]
#[sanitize(address = "off", thread = "off")]
fn f() {}
#[sanitize(realtime = "nonblocking")]
mod m {}
"#;
    compute_crate_def_map(fixture, |db, def_map| {
        let path = |segments: &[&str]| {
            ModPath::from_segments(PathKind::Plain, segments.iter().map(|it| Name::new_root(it)))
        };
        assert!(def_map.is_builtin_or_registered_attr(&path(&["sanitize"])));
        assert!(!def_map.is_builtin_or_registered_attr(&path(&["no_sanitize"])));
        assert!(def_map.diagnostics(db).is_empty(), "{:?}", def_map.diagnostics(db));
    });
}

#[test]
fn crate_def_map_super_super() {
    check(
        r#"
mod a {
    const A: usize = 0;
    mod b {
        const B: usize = 0;
        mod c {
            use super::super::*;
        }
    }
}
"#,
        expect![[r#"
            crate
            - a : type

            crate::a
            - A : value
            - b : type

            crate::a::b
            - B : value
            - c : type

            crate::a::b::c
            - A : value (glob)
            - b : type (glob)
        "#]],
    );
}

#[test]
fn crate_def_map_fn_mod_same_name() {
    check(
        r#"
mod m {
    pub mod z {}
    pub fn z() {}
}
"#,
        expect![[r#"
            crate
            - m : type

            crate::m
            - z : type value

            crate::m::z
        "#]],
    );
}

#[test]
fn bogus_paths() {
    cov_mark::check!(bogus_paths);
    check(
        r#"
//- /lib.rs
mod foo;
struct S;
use self;

//- /foo/mod.rs
use super;
use crate;
"#,
        expect![[r#"
            crate
            - S : type value
            - foo : type

            crate::foo
        "#]],
    );
}

#[test]
fn use_as() {
    check(
        r#"
//- /lib.rs
mod foo;
use crate::foo::Baz as Foo;

//- /foo/mod.rs
pub struct Baz;
"#,
        expect![[r#"
            crate
            - Foo : type (import) value (import)
            - foo : type

            crate::foo
            - Baz : type value
        "#]],
    );
}

#[test]
fn use_trees() {
    check(
        r#"
//- /lib.rs
mod foo;
use crate::foo::bar::{Baz, Quux};

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
pub struct Baz;
pub enum Quux {};
"#,
        expect![[r#"
            crate
            - Baz : type (import) value (import)
            - Quux : type (import)
            - foo : type

            crate::foo
            - bar : type

            crate::foo::bar
            - Baz : type value
            - Quux : type
        "#]],
    );
}

#[test]
fn re_exports() {
    check(
        r#"
//- /lib.rs
mod foo;
use self::foo::Baz;

//- /foo/mod.rs
pub mod bar;
pub use self::bar::Baz;

//- /foo/bar.rs
pub struct Baz;
"#,
        expect![[r#"
            crate
            - Baz : type (import) value (import)
            - foo : type

            crate::foo
            - Baz : type (import) value (import)
            - bar : type

            crate::foo::bar
            - Baz : type value
        "#]],
    );
}

#[test]
fn std_prelude() {
    cov_mark::check!(std_prelude);
    check(
        r#"
//- /main.rs crate:main deps:test_crate
#[prelude_import]
use ::test_crate::prelude::*;

use Foo::*;

//- /lib.rs crate:test_crate
pub mod prelude;

//- /prelude.rs
pub enum Foo { Bar, Baz }
"#,
        expect![[r#"
            crate
            - Bar : type (glob) value (glob)
            - Baz : type (glob) value (glob)
        "#]],
    );
}

#[test]
fn can_import_enum_variant() {
    cov_mark::check!(can_import_enum_variant);
    check(
        r#"
enum E { V }
use self::E::V;
"#,
        expect![[r#"
            crate
            - E : type
            - V : type (import) value (import)
        "#]],
    );
}

#[test]
fn edition_2015_imports() {
    check(
        r#"
//- /main.rs crate:main deps:other_crate edition:2015
mod foo;
mod bar;

//- /bar.rs
struct Bar;

//- /foo.rs
use bar::Bar;
use other_crate::FromLib;

//- /lib.rs crate:other_crate edition:2018
pub struct FromLib;
"#,
        expect![[r#"
            crate
            - bar : type
            - foo : type

            crate::bar
            - Bar : type value

            crate::foo
            - Bar : _
            - FromLib : type (import) value (import)
        "#]],
    );
}

#[test]
fn item_map_using_self() {
    check(
        r#"
//- /lib.rs
mod foo;
use crate::foo::bar::Baz::{self};

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
pub struct Baz;
"#,
        expect![[r#"
            crate
            - Baz : type (import)
            - foo : type

            crate::foo
            - bar : type

            crate::foo::bar
            - Baz : type value
        "#]],
    );
}

#[test]
fn item_map_across_crates() {
    check(
        r#"
//- /main.rs crate:main deps:test_crate
use test_crate::Baz;

//- /lib.rs crate:test_crate
pub struct Baz;
"#,
        expect![[r#"
            crate
            - Baz : type (import) value (import)
        "#]],
    );
}

#[test]
fn extern_crate_rename() {
    check(
        r#"
//- /main.rs crate:main deps:alloc
extern crate alloc as alloc_crate;
mod alloc;
mod sync;

//- /sync.rs
use alloc_crate::Arc;

//- /lib.rs crate:alloc
pub struct Arc;
"#,
        expect![[r#"
            crate
            - alloc : type
            - alloc_crate : type (extern)
            - sync : type

            crate::alloc

            crate::sync
            - Arc : type (import) value (import)
        "#]],
    );
}

#[test]
fn extern_crate_reexport() {
    check(
        r#"
//- /main.rs crate:main deps:importer
use importer::*;
use importer::extern_crate1::exported::*;
use importer::allowed_reexport::*;
use importer::extern_crate2::*;
use importer::not_allowed_reexport1;
use importer::not_allowed_reexport2;

//- /importer.rs crate:importer deps:extern_crate1,extern_crate2
extern crate extern_crate1;
extern crate extern_crate2;

pub use extern_crate1;
pub use extern_crate1 as allowed_reexport;

pub use ::extern_crate;
pub use self::extern_crate as not_allowed_reexport1;
pub use crate::extern_crate as not_allowed_reexport2;

//- /extern_crate1.rs crate:extern_crate1
pub mod exported {
    pub struct PublicItem;
    struct PrivateItem;
}

pub struct Exported;

//- /extern_crate2.rs crate:extern_crate2
pub struct NotExported;
"#,
        expect![[r#"
            crate
            - Exported : type (glob) value (glob)
            - PublicItem : type (glob) value (glob)
            - allowed_reexport : type (glob)
            - exported : type (glob)
            - not_allowed_reexport1 : _
            - not_allowed_reexport2 : _
        "#]],
    );
}

#[test]
fn extern_crate_rename_2015_edition() {
    check(
        r#"
//- /main.rs crate:main deps:alloc edition:2015
extern crate alloc as alloc_crate;
mod alloc;
mod sync;

//- /sync.rs
use alloc_crate::Arc;

//- /lib.rs crate:alloc
pub struct Arc;
"#,
        expect![[r#"
            crate
            - alloc : type
            - alloc_crate : type (extern)
            - sync : type

            crate::alloc

            crate::sync
            - Arc : type (import) value (import)
        "#]],
    );
}

#[test]
fn macro_use_extern_crate_self() {
    check(
        r#"
//- /main.rs crate:main
#[macro_use]
extern crate self as bla;
"#,
        expect![[r#"
            crate
            - bla : type (extern)
        "#]],
    );
}

#[test]
fn reexport_across_crates() {
    check(
        r#"
//- /main.rs crate:main deps:test_crate
use test_crate::Baz;

//- /lib.rs crate:test_crate
pub use foo::Baz;
mod foo;

//- /foo.rs
pub struct Baz;
"#,
        expect![[r#"
            crate
            - Baz : type (import) value (import)
        "#]],
    );
}

#[test]
fn values_dont_shadow_extern_crates() {
    check(
        r#"
//- /main.rs crate:main deps:foo
fn foo() {}
use foo::Bar;

//- /foo/lib.rs crate:foo
pub struct Bar;
"#,
        expect![[r#"
            crate
            - Bar : type (import) value (import)
            - foo : value
        "#]],
    );
}

#[test]
fn no_std_prelude() {
    check(
        r#"
        //- /main.rs edition:2018 crate:main deps:core,std
        #![cfg_attr(not(never), no_std)]
        use Rust;

        //- /core.rs crate:core
        pub mod prelude {
            pub mod rust_2018 {
                pub struct Rust;
            }
        }
        //- /std.rs crate:std deps:core
        pub mod prelude {
            pub mod rust_2018 {
            }
        }
    "#,
        expect![[r#"
            crate
            - Rust : type (import) value (import)
        "#]],
    );
}

#[test]
fn edition_specific_preludes() {
    // We can't test the 2015 prelude here since you can't reexport its contents with 2015's
    // absolute paths.

    check(
        r#"
        //- /main.rs edition:2018 crate:main deps:std
        use Rust2018;

        //- /std.rs crate:std
        pub mod prelude {
            pub mod rust_2018 {
                pub struct Rust2018;
            }
        }
    "#,
        expect![[r#"
            crate
            - Rust2018 : type (import) value (import)
        "#]],
    );
    check(
        r#"
        //- /main.rs edition:2021 crate:main deps:std
        use Rust2021;

        //- /std.rs crate:std
        pub mod prelude {
            pub mod rust_2021 {
                pub struct Rust2021;
            }
        }
    "#,
        expect![[r#"
            crate
            - Rust2021 : type (import) value (import)
        "#]],
    );
}

#[test]
fn std_prelude_takes_precedence_above_core_prelude() {
    check(
        r#"
//- /main.rs edition:2018 crate:main deps:core,std
use {Foo, Bar};

//- /std.rs crate:std deps:core
pub mod prelude {
    pub mod rust_2018 {
        pub struct Foo;
        pub use core::prelude::rust_2018::Bar;
    }
}

//- /core.rs crate:core
pub mod prelude {
    pub mod rust_2018 {
        pub struct Bar;
    }
}
"#,
        expect![[r#"
            crate
            - Bar : type (import) value (import)
            - Foo : type (import) value (import)
        "#]],
    );
}

#[test]
fn cfg_not_test() {
    check(
        r#"
//- /main.rs edition:2018 crate:main deps:std
use {Foo, Bar, Baz};

//- /lib.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        #[cfg(test)]
        pub struct Foo;
        #[cfg(not(test))]
        pub struct Bar;
        #[cfg(all(not(any()), feature = "foo", feature = "bar", opt = "42"))]
        pub struct Baz;
    }
}
"#,
        expect![[r#"
            crate
            - Bar : type (import) value (import)
            - Baz : _
            - Foo : _
        "#]],
    );
}

#[test]
fn cfg_test() {
    check(
        r#"
//- /main.rs edition:2018 crate:main deps:std
use {Foo, Bar, Baz};

//- /lib.rs crate:std cfg:test,feature=foo,feature=bar,opt=42
pub mod prelude {
    pub mod rust_2018 {
        #[cfg(test)]
        pub struct Foo;
        #[cfg(not(test))]
        pub struct Bar;
        #[cfg(all(not(any()), feature = "foo", feature = "bar", opt = "42"))]
        pub struct Baz;
    }
}
"#,
        expect![[r#"
            crate
            - Bar : _
            - Baz : type (import) value (import)
            - Foo : type (import) value (import)
        "#]],
    );
}

#[test]
fn infer_multiple_namespace() {
    check(
        r#"
//- /main.rs
mod a {
    pub type T = ();
    pub use crate::b::*;
}

use crate::a::T;

mod b {
    pub const T: () = ();
}
"#,
        expect![[r#"
            crate
            - T : type (import) value (import)
            - a : type
            - b : type

            crate::a
            - T : type value (glob)

            crate::b
            - T : value
        "#]],
    );
}

#[test]
fn underscore_import() {
    check(
        r#"
//- /main.rs
use tr::Tr as _;
use tr::Tr2 as _;

mod tr {
    pub trait Tr {}
    pub trait Tr2 {}
}
    "#,
        expect![[r#"
            crate
            - _ : type (import)
            - _ : type (import)
            - tr : type

            crate::tr
            - Tr : type
            - Tr2 : type
        "#]],
    );
}

#[test]
fn underscore_reexport() {
    check(
        r#"
//- /main.rs
mod tr {
    pub trait PubTr {}
    pub trait PrivTr {}
}
mod reex {
    use crate::tr::PrivTr as _;
    pub use crate::tr::PubTr as _;
}
use crate::reex::*;
    "#,
        expect![[r#"
            crate
            - _ : type (glob)
            - reex : type
            - tr : type

            crate::reex
            - _ : type (import)
            - _ : type (import)

            crate::tr
            - PrivTr : type
            - PubTr : type
        "#]],
    );
}

#[test]
fn underscore_pub_crate_reexport() {
    cov_mark::check!(upgrade_underscore_visibility);
    check(
        r#"
//- /main.rs crate:main deps:lib
use lib::*;

//- /lib.rs crate:lib
use tr::Tr as _;
pub use tr::Tr as _;

mod tr {
    pub trait Tr {}
}
    "#,
        expect![[r#"
            crate
            - _ : type (glob)
        "#]],
    );
}

#[test]
fn underscore_nontrait() {
    check(
        r#"
//- /main.rs
mod m {
    pub struct Struct;
    pub enum Enum {}
    pub const CONST: () = ();
}
use crate::m::{Struct as _, Enum as _, CONST as _};
    "#,
        expect![[r#"
            crate
            - m : type

            crate::m
            - CONST : value
            - Enum : type
            - Struct : type value
        "#]],
    );
}

#[test]
fn underscore_name_conflict() {
    check(
        r#"
//- /main.rs
struct Tr;

use tr::Tr as _;

mod tr {
    pub trait Tr {}
}
    "#,
        expect![[r#"
            crate
            - _ : type (import)
            - Tr : type value
            - tr : type

            crate::tr
            - Tr : type
        "#]],
    );
}

#[test]
fn cfg_the_entire_crate() {
    check(
        r#"
//- /main.rs
#![cfg(never)]

pub struct S;
pub enum E {}
pub fn f() {}
    "#,
        expect![[r#"
            crate
        "#]],
    );
}

/// rustc applies a crate's `#![cfg]` before it injects its test harness, so a test crate of
/// `#![cfg(target_arch = "wasm32")]` still has the harness's `main` (astral-experiments/blaze#35).
#[test]
fn cfg_the_entire_crate_keeps_the_appended_module() {
    use blaze_query::db::Durability;
    let mut db = fixture::db(
        r#"
//- /main.rs
#![cfg(never)]

pub fn f() {}
mod __blaze_test_harness {
    pub fn main() {}
}
"#,
    )
    .into_db();
    let krate = db.fetch_test_crate();
    let root = krate.root_file_id(&db).file_id(&db);
    // The same module written only in source follows the crate's cfg.
    expect![[r#"
        crate
    "#]]
    .assert_eq(&crate_def_map(&db, krate).dump(&db));
    db.set_appended_root_module(root, Some("__blaze_test_harness"), Durability::LOW);
    expect![[r#"
        crate
        - __blaze_test_harness : type

        crate::__blaze_test_harness
        - main : value
    "#]]
    .assert_eq(&crate_def_map(&db, krate).dump(&db));
    db.set_appended_root_module(root, None, Durability::LOW);
    expect![[r#"
        crate
    "#]]
    .assert_eq(&crate_def_map(&db, krate).dump(&db));
}

#[test]
fn use_crate_as() {
    check(
        r#"
use crate as foo;

use foo::bar as baz;

fn bar() {}
        "#,
        expect![[r#"
            crate
            - bar : value
            - baz : value (import)
            - foo : type (import)
        "#]],
    );
}

#[test]
fn self_imports_only_types() {
    check(
        r#"
//- /main.rs
mod m {
    pub macro S() {}
    pub struct S;
}

use self::m::S::{self};
    "#,
        expect![[r#"
            crate
            - S : type (import)
            - m : type

            crate::m
            - S : type value macro!
        "#]],
    );
}

#[test]
fn import_from_extern_crate_only_imports_public_items() {
    check(
        r#"
//- /lib.rs crate:lib deps:settings,macros
use macros::settings;
use settings::Settings;
//- /settings.rs crate:settings
pub struct Settings;
//- /macros.rs crate:macros
mod settings {}
pub const settings: () = ();
        "#,
        expect![[r#"
            crate
            - Settings : type (import) value (import)
            - settings : value (import)
        "#]],
    )
}

#[test]
fn non_prelude_deps() {
    check(
        r#"
//- /lib.rs crate:lib deps:dep extern-prelude:
use dep::Struct;
//- /dep.rs crate:dep
pub struct Struct;
        "#,
        expect![[r#"
            crate
            - Struct : _
        "#]],
    );
    check(
        r#"
//- /lib.rs crate:lib deps:dep extern-prelude:
extern crate dep;
use dep::Struct;
//- /dep.rs crate:dep
pub struct Struct;
        "#,
        expect![[r#"
            crate
            - Struct : type (import) value (import)
            - dep : type (extern)
        "#]],
    );
}

#[test]
fn braced_supers_in_use_tree() {
    cov_mark::check!(concat_super_mod_paths);
    check(
        r#"
mod some_module {
    pub fn unknown_func() {}
}

mod other_module {
    mod some_submodule {
        use { super::{ super::unknown_func, }, };
    }
}

use some_module::unknown_func;
        "#,
        expect![[r#"
            crate
            - other_module : type
            - some_module : type
            - unknown_func : value (import)

            crate::other_module
            - some_submodule : type

            crate::other_module::some_submodule
            - unknown_func : value (import)

            crate::some_module
            - unknown_func : value
        "#]],
    )
}

#[test]
fn diagnostics_of_a_module_are_its_own_in_push_order() {
    compute_crate_def_map(
        r#"
//- /lib.rs
mod a;
mod b;
mod c;
use missing::one;
use a::missing_two;
//- /a.rs
use missing::three;
use crate::missing_four;
//- /b.rs
use missing::five;
//- /c.rs
"#,
        |db, def_map| {
            let mut seen = 0;
            let mut with_any = 0;
            for (module, _) in def_map.modules(db) {
                let filtered: Vec<_> =
                    def_map.diagnostics(db).iter().filter(|it| it.in_module == module).collect();
                let of: Vec<_> = def_map.diagnostics_of(db, module).collect();
                assert_eq!(of, filtered);
                seen += of.len();
                with_any += usize::from(!of.is_empty());
            }
            assert_eq!(seen, def_map.diagnostics(db).len());
            assert_eq!(with_any, 3);
        },
    );
}

#[test]
fn impls_keep_their_order_and_their_reservation_bit() {
    use crate::{
        ImplId, Lookup,
        attrs::AttrFlags,
        nameres::frozen::{self, Frozen, Mode},
        src::HasSource,
    };
    use syntax::view::AstNode;

    let mut db = fixture::db(
        r#"
trait Tr {}
struct A;
struct B;
struct C;
struct D;
impl Tr for A {}
#[rustc_reservation_impl = "reserved"]
impl Tr for B {}
impl B {}
#[cfg_attr(all(), rustc_reservation_impl = "reserved by cfg_attr")]
impl Tr for C {}
#[cfg_attr(any(), rustc_reservation_impl = "not reserved")]
impl Tr for D {}
"#,
    )
    .into_db();
    let krate = db.fetch_test_crate();
    {
        let def_map = crate_def_map(&db, krate);
        let scope = def_map.module(&db, def_map.root).scope(&db);
        let start =
            |it: ImplId| it.lookup(&db).source_view(&db).value.syntax().text_range().start();
        let impls: Vec<ImplId> = scope.impls().collect();
        assert_eq!(impls.len(), 5);
        assert!(impls.windows(2).all(|it| start(it[0]) < start(it[1])), "declaration order");

        let reserved = |it: &ImplId| {
            AttrFlags::query(&db, (*it).into()).contains(AttrFlags::RUSTC_RESERVATION_IMPL)
        };
        let trait_impls: Vec<ImplId> = scope.trait_impls().collect();
        let expected: Vec<ImplId> =
            trait_impls.iter().filter(|it| !reserved(it)).copied().collect();
        assert_eq!(trait_impls.len(), 4);
        assert_eq!(expected.len(), 2);
        assert_eq!(scope.trait_impls_without_reservation().collect::<Vec<_>>(), expected);
        assert_eq!(scope.inherent_impls().count(), 1);
    }

    let bytes = frozen::encode(&db, krate).unwrap();
    let frozen = Frozen { bytes: bytes.into(), mode: Mode::Verify };
    frozen::set(&mut db, [(krate, frozen)].into_iter().collect());
    let def_map = crate_def_map(&db, krate);
    let scope = def_map.module(&db, def_map.root).scope(&db);
    assert_eq!(scope.trait_impls_without_reservation().count(), 2);
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

#[test]
fn an_adts_variants_are_itself_or_its_enums_in_order() {
    use hir_expand::name::Name;
    use intern::Symbol;

    use crate::{AdtId, ModuleDefId, VariantId};

    let name = |s: &str| Name::new_symbol_root(Symbol::intern(s));
    compute_crate_def_map(
        r#"
struct S;
union U { a: u8 }
enum E { A, B(u8), C {} }
enum Empty {}
"#,
        |db, def_map| {
            let scope = def_map.module(db, def_map.root).scope(db);
            let adt = |s: &str| match scope.get(&name(s)).take_types() {
                Some(ModuleDefId::AdtId(it)) => it,
                it => panic!("`{s}` is not an adt: {it:?}"),
            };
            let variants = |s: &str| adt(s).variants(db).collect::<Vec<_>>();

            let AdtId::StructId(s) = adt("S") else { panic!("`S` is a struct") };
            assert_eq!(variants("S"), [VariantId::StructId(s)]);
            let AdtId::UnionId(u) = adt("U") else { panic!("`U` is a union") };
            assert_eq!(variants("U"), [VariantId::UnionId(u)]);
            let AdtId::EnumId(e) = adt("E") else { panic!("`E` is an enum") };
            let declared = &e.enum_variants(db).variants;
            let of = |s: &str| VariantId::EnumVariantId(declared[&name(s)].0);
            assert_eq!(variants("E"), [of("A"), of("B"), of("C")]);
            assert_eq!(variants("Empty"), Vec::<VariantId>::new());
        },
    );
}

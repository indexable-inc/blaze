//! What name resolution records of how names resolved: the imports each path used and how
//! (`used_imports`), the bindings a scope keeps beside the one a name has, and how macro calls
//! and derive helpers resolved.

use base_db::{Crate, all_crates};
use expect_test::{Expect, expect};
use hir_expand::name::Name;
use intern::sym;
use syntax::{
    ast,
    view::{self, AstNode as _},
};

use super::*;
use crate::{
    AssocItemId, Lookup, ModuleDefId, ModuleId, TraitId,
    item_scope::{GlobId, ImportId, ImportOrExternCrate, ItemInNs},
    nameres::{
        assoc::ImplItems,
        uses::{MacroVia, NameRecords, UsedImports, used_imports, used_imports_with},
    },
    per_ns::Namespace,
    resolver::{HasResolver, Resolver},
    src::use_tree_to_view,
    test_db::TestDBExt,
};

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// The source of an import or `extern crate`: its use tree, or the item.
fn import_text(db: &TestDB, import: ImportOrExternCrate) -> String {
    match import {
        ImportOrExternCrate::Import(ImportId { use_, idx })
        | ImportOrExternCrate::Glob(GlobId { use_, idx }) => {
            use_tree_to_view(db, use_.lookup(db).id, idx).syntax().text().to_owned()
        }
        ImportOrExternCrate::ExternCrate(it) => {
            it.lookup(db).id.to_view(db).syntax().text().to_owned()
        }
    }
}

/// [`used_imports`] of `krate`, a line for each import, `#[macro_use]` and ambiguous use,
/// sorted.
pub(super) fn render_used(db: &TestDB, krate: Crate) -> String {
    render(db, used_imports(db, krate))
}

/// [`used_imports_with`] the uses of each trait of `traits` found from `resolver`, as type
/// inference records a method call of it.
pub(super) fn render_used_with_traits(
    db: &TestDB,
    krate: Crate,
    resolver: &Resolver<'_>,
    traits: impl IntoIterator<Item = TraitId>,
) -> String {
    let mut more = Vec::new();
    for it in traits {
        resolver.record_trait_uses(db, it, &mut more);
    }
    render(db, &used_imports_with(db, krate, &more))
}

fn render(db: &TestDB, used: &UsedImports) -> String {
    let mut lines = Vec::new();
    for (import, how) in used.imports() {
        let mut line = format!("{} {how:?}", import_text(db, import));
        if let ImportOrExternCrate::Glob(glob) = import {
            let names: Vec<&str> = used.glob_names(glob).iter().map(Name::as_str).collect();
            line += &format!(" [{}]", names.join(", "));
        }
        lines.push(line);
    }
    for (extern_crate, how) in used.macro_uses() {
        let text = import_text(db, ImportOrExternCrate::ExternCrate(extern_crate));
        lines.push(format!("macro_use {text} {how:?}"));
    }
    for &(glob, ref name, ns) in used.ambiguous() {
        let text = import_text(db, ImportOrExternCrate::Glob(glob));
        lines.push(format!("ambiguous {} {ns:?} through {text}", name.as_str()));
    }
    lines.sort();
    lines.iter().map(|it| format!("{it}\n")).collect()
}

/// The records of `def_map`, each in its order, naming everything by its source.
pub(super) fn render_records(db: &TestDB, def_map: &DefMap) -> String {
    let NameRecords { uses, unresolved, glob_targets, macro_calls, derive_helpers, .. } =
        def_map.name_records(db);
    let mut out = String::new();
    for it in uses {
        let text = import_text(db, it.import);
        out += &format!("use {} {:?} {:?} {text}\n", it.name.as_str(), it.ns, it.used);
    }
    for &it in unresolved {
        out += &format!("unresolved {}\n", import_text(db, it));
    }
    for &(glob, module) in glob_targets {
        let target = module.name(db).map_or("crate".to_owned(), |it| it.as_str().to_owned());
        out += &format!("glob {} reads {target}\n", import_text(db, ImportOrExternCrate::Glob(glob)));
    }
    for it in macro_calls {
        out += &format!("call {}\n", via_text(db, it.via));
    }
    for it in derive_helpers {
        out += &format!("helper on {} before {}\n", item_name(db, it.item), it.before_derive);
    }
    out
}

fn via_text(db: &TestDB, via: MacroVia) -> String {
    match via {
        MacroVia::Textual => "textual".to_owned(),
        MacroVia::Path(None) => "declaration".to_owned(),
        MacroVia::Path(Some(it)) => format!("import {}", import_text(db, it)),
        MacroVia::MacroUse(None) => "macro_use of the prelude's crate".to_owned(),
        MacroVia::MacroUse(Some(it)) => {
            format!("macro_use {}", import_text(db, ImportOrExternCrate::ExternCrate(it)))
        }
        MacroVia::Prelude => "prelude".to_owned(),
    }
}

fn item_name(db: &TestDB, item: crate::AstId<ast::Item>) -> String {
    let node = item.to_view(db);
    let name = node.syntax().descendants().find_map(view::Name::cast).unwrap();
    name.text().to_owned()
}

fn check_used(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = fixture::db(ra_fixture).into_db();
    expect.assert_eq(&render_used(&db, db.fetch_test_crate()));
}

fn check_used_in(name: &str, #[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = fixture::db(ra_fixture).into_db();
    expect.assert_eq(&render_used(&db, crate_named(&db, name)));
}

fn check_records_in(name: &str, #[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = fixture::db(ra_fixture).into_db();
    let def_map = crate_def_map(&db, crate_named(&db, name));
    expect.assert_eq(&render_records(&db, def_map));
}

#[test]
fn a_glob_records_the_names_used_through_it_only() {
    check_used(
        r#"
mod a { pub struct S; pub struct T; }
use a::*;
fn f(_: S) {}
"#,
        expect![[r#"
            a::* Scope [S]
        "#]],
    );
}

#[test]
fn two_globs_giving_a_name_different_items_keep_both() {
    let fixture = r#"
mod a { pub fn x() {} }
mod b { pub fn x() {} }
use a::*;
use b::*;
fn f() { x(); }
"#;
    check_used(
        fixture,
        expect![[r#"
            a::* Scope [x]
            ambiguous x Values through a::*
        "#]],
    );
    let db = fixture::db(fixture).into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let x = Name::new_symbol_root(intern::Symbol::intern("x"));
    let bound = scope.get(&x).values.unwrap();
    let [other] = scope.glob_ambiguities() else { panic!("{:?}", scope.glob_ambiguities()) };
    assert_eq!(scope.glob_ambiguity(&x, Namespace::Values), Some(other));
    let ItemInNs::Values(other_def) = other.other else { panic!("{other:?}") };
    assert_ne!(other_def, bound.def);
    assert_ne!(Some(ImportOrExternCrate::Glob(other.glob)), bound.import.map(Into::into));
    assert_eq!(import_text(&db, ImportOrExternCrate::Glob(other.glob)), "b::*");
}

#[test]
fn an_import_is_visible_as_the_prelude_gives_it_too() {
    let db = fixture::db(
        r#"
//- minicore: option
use core::option::Option;
"#,
    )
    .into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let option = Name::new_symbol_root(sym::Option);
    let (import, _) = scope.import_of(&option, Namespace::Types).unwrap();
    assert_eq!(import_text(&db, import), "core::option::Option");
    let in_prelude = def_map.resolve_in_prelude(&db, &option).types.unwrap();
    assert_eq!(scope.get(&option).types.unwrap().def, in_prelude.def);
    assert!(matches!(in_prelude.def, ModuleDefId::AdtId(_)));
    assert_eq!(scope.import_of(&option, Namespace::Values), None);
}

#[test]
fn a_first_segment_is_found_in_scope_the_others_relative() {
    check_used(
        r#"
mod m { pub mod n { pub struct S; } pub struct T; }
use m::n;
use m::T as A;
use self::m::T as B;
fn f(_: n::S, _: A, _: self::B) {}
"#,
        expect![[r#"
            m::T as A Scope
            m::n Scope
            self::m::T as B Other
        "#]],
    );
}

#[test]
fn a_glob_read_through_another_glob_uses_both() {
    check_used(
        r#"
mod a { pub struct S; }
mod b { pub use super::a::*; }
use b::*;
fn f(_: S) {}
"#,
        expect![[r#"
            b::* Scope [S]
            super::a::* Other [S]
        "#]],
    );
}

#[test]
fn an_import_read_through_another_import_uses_both() {
    check_used(
        r#"
mod a { pub struct S; }
mod b { pub use super::a::S; }
use b::S;
fn f(_: S) {}
"#,
        expect![[r#"
            b::S Scope
            super::a::S Other
        "#]],
    );
}

#[test]
fn an_import_no_path_names_is_not_used() {
    check_used(
        r#"
mod a { pub struct S; pub struct T; }
use a::{S, T};
fn f(_: S) {}
"#,
        expect![[r#"
            S Scope
        "#]],
    );
}

#[test]
fn an_unresolved_import_counts_as_used() {
    check_used(
        r#"
use missing::thing;
"#,
        expect![[r#"
            missing::thing Other
        "#]],
    );
}

#[test]
fn an_extern_crate_the_extern_prelude_has_anyway_is_used_only_relative() {
    check_used_in(
        "main",
        r#"
//- /main.rs crate:main deps:dep
extern crate dep;
extern crate dep as renamed;
fn f(_: dep::S, _: renamed::S, _: crate::dep::S) {}
//- /dep.rs crate:dep
pub struct S;
"#,
        expect![[r#"
            extern crate dep as renamed; Scope
            extern crate dep; Other
        "#]],
    );
}

#[test]
fn a_macro_of_the_macro_use_prelude_uses_its_extern_crate() {
    let fixture = r#"
//- /main.rs crate:main deps:dep
#[macro_use]
extern crate dep;
made!();
//- /dep.rs crate:dep
#[macro_export]
macro_rules! made { () => {} }
"#;
    check_used_in(
        "main",
        fixture,
        expect![[r#"
            macro_use #[macro_use]
            extern crate dep; Scope
        "#]],
    );
    let db = fixture::db(fixture).into_db();
    let def_map = crate_def_map(&db, crate_named(&db, "main"));
    let [call] = def_map.name_records(&db).macro_calls() else { panic!() };
    assert_eq!(via_text(&db, call.via), "macro_use #[macro_use]\nextern crate dep;");
}

#[test]
fn a_macro_call_records_how_its_name_was_found() {
    let fixture = r#"
macro_rules! here { () => {} }
here!();
mod a {
    #[macro_export]
    macro_rules! there { () => {} }
}
use crate::there as again;
again!();
crate::there!();
"#;
    check_used(
        fixture,
        expect![[r#"
            crate::there as again Scope
        "#]],
    );
    let db = fixture::db(fixture).into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let mut vias: Vec<String> =
        def_map.name_records(&db).macro_calls().iter().map(|it| via_text(&db, it.via)).collect();
    vias.sort();
    assert_eq!(vias, ["declaration", "import crate::there as again", "textual"]);
}

#[test]
fn a_derive_helper_before_its_derive_resolves_as_rustc_s_compat_helper() {
    let db = fixture::db(
        r#"
//- /main.rs crate:main deps:proc
#[rustc_builtin_macro]
pub macro derive($item:item) {}

#[helper]
#[derive(proc::Derive)]
struct Before;

#[derive(proc::Derive)]
#[helper]
struct After;

//- /proc.rs crate:proc
#![crate_type="proc-macro"]
#[proc_macro_derive(Derive, attributes(helper))]
fn derive() {}
"#,
    )
    .into_db();
    let def_map = crate_def_map(&db, crate_named(&db, "main"));
    let mut helpers: Vec<(String, bool)> = (def_map.name_records(&db).derive_helpers().iter())
        .map(|it| (item_name(&db, it.item), it.before_derive))
        .collect();
    helpers.sort();
    assert_eq!(helpers, [("After".to_owned(), false), ("Before".to_owned(), true)]);
    let dump = def_map.dump(&db);
    assert!(dump.contains("- After : type value") && dump.contains("- Before : type value"));
    let unresolved = def_map.diagnostics(&db).iter().map(|it| format!("{:?}", it.kind));
    assert_eq!(unresolved.collect::<Vec<_>>(), Vec::<String>::new());
}

#[test]
fn the_records_name_what_a_def_map_resolved_through() {
    check_records_in(
        "main",
        r#"
//- /main.rs crate:main deps:dep
#[macro_use]
extern crate dep;
mod a { pub struct S; }
mod b { pub use super::a::*; }
use b::S;
use missing::thing;
made!();
//- /dep.rs crate:dep
#[macro_export]
macro_rules! made { () => {} }
"#,
        expect![[r#"
            use S Types Other super::a::*
            use S Values Other super::a::*
            use made Macros Scope #[macro_use]
            extern crate dep;
            unresolved missing::thing
            glob super::a::* reads a
            call macro_use #[macro_use]
            extern crate dep;
        "#]],
    );
}

#[test]
fn an_underscore_import_and_a_shadowed_one_are_kept() {
    let db = fixture::db(
        r#"
mod m { pub trait Tr {} pub struct S; }
use m::Tr as _;
struct S;
use m::S;
"#,
    )
    .into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let [underscore] = scope.underscore_imports() else { panic!() };
    let import = ImportOrExternCrate::Import(underscore.import);
    assert_eq!(import_text(&db, import), "m::Tr as _");
    assert!(matches!(underscore.def, Some(ModuleDefId::TraitId(_))));
    let shadowed: Vec<_> = scope
        .shadowed_imports()
        .iter()
        .map(|it| (import_text(&db, it.import), it.name.as_str().to_owned(), it.item))
        .collect();
    let s = Name::new_symbol_root(intern::Symbol::intern("S"));
    let declared = scope.get(&s);
    assert_eq!(declared.types.unwrap().import, None);
    assert_eq!(shadowed.len(), 2, "{shadowed:?}");
    for (text, name, item) in shadowed {
        assert_eq!((text.as_str(), name.as_str()), ("m::S", "S"));
        let def = match item {
            ItemInNs::Types(it) | ItemInNs::Values(it) => it,
            ItemInNs::Macros(_) => panic!(),
        };
        assert_ne!(Some(def), declared.types.map(|it| it.def));
    }
}

fn name(text: &str) -> Name {
    Name::new_symbol_root(intern::Symbol::intern(text))
}

/// The module `path` (names from the crate root) of `krate`.
fn module_at(db: &TestDB, krate: Crate, path: &[&str]) -> ModuleId {
    let def_map = crate_def_map(db, krate);
    let mut at = def_map.root;
    for segment in path {
        at = def_map.module(db, at).children(db)[&name(segment)];
    }
    at
}

fn trait_at(db: &TestDB, krate: Crate, path: &[&str], trait_name: &str) -> TraitId {
    let module = module_at(db, krate, path);
    let scope = module.def_map(db).module(db, module).scope(db);
    match scope.get(&name(trait_name)).types.unwrap().def {
        ModuleDefId::TraitId(it) => it,
        other => panic!("{other:?}"),
    }
}

fn root_fn(db: &TestDB, krate: Crate, fn_name: &str) -> crate::FunctionId {
    let module = module_at(db, krate, &[]);
    let scope = module.def_map(db).module(db, module).scope(db);
    match scope.get(&name(fn_name)).values.unwrap().def {
        ModuleDefId::FunctionId(it) => it,
        other => panic!("{other:?}"),
    }
}

/// The uses of the test crate with a method of trait `trait_path::trait_name` called in the
/// root function `in_fn`.
fn check_trait_used(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    trait_path: &[&str],
    trait_name: &str,
    expect: Expect,
) {
    let db = fixture::db(ra_fixture).into_db();
    let krate = db.fetch_test_crate();
    let tr = trait_at(&db, krate, trait_path, trait_name);
    let resolver = root_fn(&db, krate, "f").resolver(&db);
    expect.assert_eq(&render_used_with_traits(&db, krate, &resolver, [tr]));
}

#[test]
fn a_method_of_an_imported_trait_uses_the_import() {
    check_trait_used(
        r#"
mod m { pub trait Tr { fn go(&self) {} } impl Tr for () {} }
use m::Tr;
fn f() { ().go(); }
"#,
        &["m"],
        "Tr",
        expect![[r#"
            m::Tr Other
        "#]],
    );
}

#[test]
fn a_method_of_an_unnamed_trait_read_through_a_glob_uses_the_glob_and_the_import() {
    let fixture = r#"
mod m { pub trait Tr { fn go(&self) {} } impl Tr for () {} }
mod p { pub use super::m::Tr as _; }
use p::*;
fn f() { ().go(); }
"#;
    check_used(fixture, expect![[""]]);
    check_trait_used(
        fixture,
        &["m"],
        "Tr",
        expect![[r#"
            p::* Other [_]
            super::m::Tr as _ Other
        "#]],
    );
    let db = fixture::db(fixture).into_db();
    let krate = db.fetch_test_crate();
    let root = module_at(&db, krate, &[]);
    let scope = root.def_map(&db).module(&db, root).scope(&db);
    let tr = trait_at(&db, krate, &["m"], "Tr");
    let Some((bound, Some(ImportOrExternCrate::Glob(glob)))) = scope.trait_binding(tr) else {
        panic!("{:?}", scope.trait_binding(tr))
    };
    assert_eq!(bound.as_str(), "_");
    assert_eq!(import_text(&db, ImportOrExternCrate::Glob(glob)), "p::*");
}

#[test]
fn a_method_of_a_named_trait_read_through_globs_uses_each() {
    check_trait_used(
        r#"
mod m { pub trait Tr { fn go(&self) {} } impl Tr for () {} }
mod p { pub use super::m::*; }
use p::*;
fn f() { ().go(); }
"#,
        &["m"],
        "Tr",
        expect![[r#"
            p::* Other [Tr]
            super::m::* Other [Tr]
        "#]],
    );
}

#[test]
fn a_trait_read_through_globs_that_import_each_other_ends() {
    check_trait_used(
        r#"
mod m { pub trait Tr { fn go(&self) {} } impl Tr for () {} }
mod a { pub use super::b::*; pub use super::m::Tr as _; }
mod b { pub use super::a::*; }
use b::*;
fn f() { ().go(); }
"#,
        &["m"],
        "Tr",
        expect![[r#"
            b::* Other [_]
            super::a::* Other [_]
            super::m::Tr as _ Other
        "#]],
    );
}

#[test]
fn a_trait_declared_where_it_is_found_uses_no_import() {
    check_trait_used(
        r#"
mod m { pub trait Tr { fn go(&self) {} } impl Tr for () {} }
use m::*;
trait Tr { fn go(&self) {} }
impl Tr for () {}
fn f() { ().go(); }
"#,
        &[],
        "Tr",
        expect![[""]],
    );
}

#[test]
fn a_trait_of_the_std_prelude_uses_no_import() {
    let db = fixture::db(
        r#"
//- minicore: clone
fn f() { ().clone(); }
"#,
    )
    .into_db();
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let ModuleDefId::TraitId(clone) = def_map.resolve_in_prelude(&db, &name("Clone")).types.unwrap().def
    else {
        panic!()
    };
    let resolver = root_fn(&db, krate, "f").resolver(&db);
    expect![[""]].assert_eq(&render_used_with_traits(&db, krate, &resolver, [clone]));
}

#[test]
fn the_impl_s_own_trait_uses_no_import() {
    let db = fixture::db(
        r#"
mod m { pub trait Tr { fn go(&self) {} } }
use m::*;
struct S;
impl m::Tr for S { fn go(&self) { self.go() } }
fn f() {}
"#,
    )
    .into_db();
    let krate = db.fetch_test_crate();
    let tr = trait_at(&db, krate, &["m"], "Tr");
    let root = module_at(&db, krate, &[]);
    let scope = root.def_map(&db).module(&db, root).scope(&db);
    let impl_ = scope.impls().next().unwrap();
    let [(_, AssocItemId::FunctionId(go))] = &*ImplItems::of(&db, impl_).0.items else { panic!() };
    let resolver = go.resolver(&db);
    expect![[""]].assert_eq(&render_used_with_traits(&db, krate, &resolver, [tr]));
    let in_fn = root_fn(&db, krate, "f").resolver(&db);
    expect![[r#"
        m::* Other [Tr]
    "#]]
    .assert_eq(&render_used_with_traits(&db, krate, &in_fn, [tr]));
}

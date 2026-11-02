use base_db::{Crate, all_crates};
use intern::Symbol;
use syntax::view::HasName;
use hir_expand::fixture;

use super::{crate_diagnostic_items, diagnostic_items};
use crate::{
    AdtId, AttrDefId, DefWithBodyId, Lookup, MacroId,
    attrs::AttrFlags,
    nameres::demand::{Look, macros_writing_diagnostic_items, walk_bodies_saying},
    src::HasSource,
    test_db::TestDB,
};

/// A diagnostic item in every place rustc's walk finds one: an enum, a module, a function in
/// it, a trait and its method, an inherent impl's method, a `cfg_attr`, a foreign function, a
/// `macro_rules!` exported and not, an item a macro writes, and items declared in bodies, one
/// written there and one by a macro.
const ITEMS: &str = r#"
//- /core.rs crate:core
#![feature(rustc_attrs)]
#[rustc_diagnostic_item = "Option"]
pub enum Option<T> { None, Some(T) }
#[rustc_diagnostic_item = "core_mem"]
pub mod mem {
    #[rustc_diagnostic_item = "mem_zeroed"]
    pub fn zeroed() {}
}
#[rustc_diagnostic_item = "Clone"]
pub trait Clone {
    #[rustc_diagnostic_item = "clone_fn"]
    fn clone(&self);
}
pub struct S;
impl S {
    #[rustc_diagnostic_item = "s_new"]
    pub fn new() {}
}
#[cfg_attr(all(), rustc_diagnostic_item = "via_cfg_attr")]
pub fn on() {}
#[cfg_attr(any(), rustc_diagnostic_item = "off")]
pub fn off() {}
extern "C" {
    #[rustc_diagnostic_item = "foreign"]
    pub fn foreign();
}
#[rustc_diagnostic_item = "local_macro"]
macro_rules! local { () => {} }
#[macro_export]
#[rustc_diagnostic_item = "exported_macro"]
macro_rules! exported { () => {} }
macro_rules! make { ($name:literal) => { #[rustc_diagnostic_item = $name] pub fn made() {} } }
make!("made_fn");
pub fn body() {
    #[rustc_diagnostic_item = "in_body"]
    fn hidden() {}
}
pub fn calls() { make!("in_body_by_macro"); }
pub fn plain() { struct P; impl P {} }
//- /std.rs crate:std deps:core
#![feature(rustc_attrs)]
#[rustc_diagnostic_item = "HashMap"]
pub struct HashMap;
//- /main.rs crate:main deps:std
#[rustc_diagnostic_item = "main_item"]
pub fn mine() {}
"#;

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name)
        })
        .unwrap_or_else(|| panic!("no crate {name}"))
}

fn function_name(db: &TestDB, owner: DefWithBodyId) -> String {
    match owner {
        DefWithBodyId::FunctionId(it) => {
            it.lookup(db).source_view(db).value.name().unwrap().text().to_owned()
        }
        other => panic!("not a function: {other:?}"),
    }
}

fn names(items: &super::DiagnosticItems) -> Vec<String> {
    let mut names: Vec<_> = items.by_name.keys().map(|it| it.as_str().to_owned()).collect();
    names.sort();
    names
}

#[test]
fn every_owner_is_found() {
    let db = fixture::db(ITEMS).into_db();
    let core = crate_named(&db, "core");
    let items = crate_diagnostic_items(&db, core);
    assert_eq!(
        names(items),
        [
            "Clone",
            "Option",
            "clone_fn",
            "core_mem",
            "exported_macro",
            "foreign",
            "in_body",
            "in_body_by_macro",
            "local_macro",
            "made_fn",
            "mem_zeroed",
            "s_new",
            "via_cfg_attr",
        ]
    );
    assert!(items.duplicates.is_empty(), "{:?}", items.duplicates);
    for (name, &owner) in &items.by_name {
        assert_eq!(AttrFlags::diagnostic_item(&db, owner).as_ref(), Some(name));
    }
    let get = |name: &str| items.get(&Symbol::intern(name)).unwrap();
    assert!(matches!(get("Option"), AttrDefId::AdtId(AdtId::EnumId(_))));
    assert!(matches!(get("core_mem"), AttrDefId::ModuleId(_)));
    assert!(matches!(get("mem_zeroed"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("Clone"), AttrDefId::TraitId(_)));
    assert!(matches!(get("clone_fn"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("s_new"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("foreign"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("local_macro"), AttrDefId::MacroId(MacroId::MacroRulesId(_))));
    assert!(matches!(get("exported_macro"), AttrDefId::MacroId(MacroId::MacroRulesId(_))));
    assert!(matches!(get("in_body"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("in_body_by_macro"), AttrDefId::FunctionId(_)));
}

#[test]
fn only_bodies_that_may_say_one_are_lowered() {
    let db = fixture::db(ITEMS).into_db();
    let core = crate_named(&db, "core");
    let macros: Vec<_> =
        macros_writing_diagnostic_items(&db, core).iter().map(|it| it.as_str()).collect();
    assert_eq!(macros, ["make"]);
    let mut lowered = Vec::new();
    walk_bodies_saying(&db, core, Look::DiagnosticItems, |owner, _| {
        lowered.push(function_name(&db, owner))
    });
    lowered.sort();
    assert_eq!(lowered, ["body", "calls"]);
}

#[test]
fn a_crate_sees_its_dependencies_items() {
    let db = fixture::db(ITEMS).into_db();
    let main = crate_named(&db, "main");
    let items = diagnostic_items(&db, main);
    let names = names(items);
    for name in ["main_item", "HashMap", "Option", "mem_zeroed", "in_body"] {
        assert!(names.iter().any(|it| it == name), "{name} missing from {names:?}");
    }
    assert!(items.duplicates.is_empty(), "{:?}", items.duplicates);
    assert_eq!(names.len(), 15);
    // `std` does not see `main`'s.
    let std = crate_named(&db, "std");
    assert!(diagnostic_items(&db, std).get(&Symbol::intern("main_item")).is_none());
}

#[test]
fn a_name_given_twice_keeps_the_nearest() {
    let db = fixture::db(
        r#"
//- /core.rs crate:core
#[rustc_diagnostic_item = "Twice"]
pub struct InCore;
//- /lib.rs crate:lib deps:core
#[rustc_diagnostic_item = "Twice"]
pub struct InLib;
#[rustc_diagnostic_item = "Again"]
pub fn first() {}
#[rustc_diagnostic_item = "Again"]
pub fn second() {}
"#,
    )
    .into_db();
    let lib = crate_named(&db, "lib");
    let own = crate_diagnostic_items(&db, lib);
    assert_eq!(own.duplicates.len(), 1, "{:?}", own.duplicates);
    let (name, kept, other) = &own.duplicates[0];
    assert_eq!(name.as_str(), "Again");
    assert_ne!(kept, other);
    assert_eq!(own.get(name), Some(*kept));

    let all = diagnostic_items(&db, lib);
    let twice = Symbol::intern("Twice");
    let in_lib = own.get(&twice).unwrap();
    assert_eq!(all.get(&twice), Some(in_lib));
    let across: Vec<_> = all.duplicates.iter().filter(|(it, _, _)| *it == twice).collect();
    assert_eq!(across.len(), 1, "{:?}", all.duplicates);
    assert_eq!(across[0].1, in_lib);
    let core = crate_named(&db, "core");
    assert_eq!(Some(across[0].2), crate_diagnostic_items(&db, core).get(&twice));
}

//! The imports of traits a crate's inference picked through, rustc's `used_trait_imports`.

use base_db::{Crate, Db, all_crates};
use hir_def::{
    Lookup as _,
    item_scope::{GlobId, ImportId, ImportOrExternCrate},
    src::use_tree_to_view,
};
use hir_expand::fixture;

use super::used_imports_with_traits;

const FIXTURE: &str = r#"
//- /lib.rs crate:main
mod m {
    pub trait Method { fn method(&self) {} }
    impl Method for u8 {}
    pub trait Assoc { fn assoc() {} }
    impl Assoc for u8 {}
    pub trait Pattern { const C: u8; }
    impl Pattern for u8 { const C: u8 = 0; }
    pub trait Unused { fn other(&self) {} }
    impl Unused for u8 {}
}
use m::Method;
use m::Assoc;
use m::Unused;
fn by_method() {
    0u8.method();
}
fn by_path() {
    use m::Assoc as _;
    u8::assoc();
}
fn by_pattern(x: u8) {
    use m::Pattern;
    match x {
        u8::C => {}
        _ => {}
    }
}
"#;

fn main_crate(db: &Db) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == "main")
        })
        .unwrap()
}

#[test]
fn the_innermost_import_of_each_picked_trait_is_used() {
    let db = fixture::db(FIXTURE).into_db();
    let used = used_imports_with_traits(&db, main_crate(&db));
    let mut texts: Vec<String> = used
        .imports()
        .filter_map(|(import, _)| match import {
            ImportOrExternCrate::Import(ImportId { use_, idx })
            | ImportOrExternCrate::Glob(GlobId { use_, idx }) => {
                Some(use_tree_to_view(&db, use_.lookup(&db).id, idx).to_string())
            }
            ImportOrExternCrate::ExternCrate(_) => None,
        })
        .collect();
    texts.sort();
    // The block's `use m::Assoc as _` is the nearer import of `Assoc`, so the module's is unused;
    // `Unused` is implemented for `u8` but never picked.
    assert_eq!(texts, ["m::Assoc as _", "m::Method", "m::Pattern"]);
}

/// The fixture's `/lib.rs` with `by_method`'s body changed to `body`.
fn with_by_method(body: &str) -> String {
    let start = FIXTURE.find("//- /lib.rs crate:main\n").expect("the fixture's lib.rs")
        + "//- /lib.rs crate:main\n".len();
    let text = &FIXTURE[start..];
    let old = "fn by_method() {\n    0u8.method();\n}";
    assert!(text.contains(old), "the fixture has `by_method`");
    text.replace(old, &format!("fn by_method() {{\n    {body}\n}}"))
}

/// What a read of the crate's used imports executes after `lib.rs` becomes `text`.
fn executed_after(text: &str) -> Vec<String> {
    let mut fixture = fixture::db(FIXTURE);
    let krate = main_crate(fixture.db());
    let _ = used_imports_with_traits(fixture.db(), krate);
    let file = fixture.file_at("/lib.rs").file_id(fixture.db());
    _ = fixture.edit(|edit| _ = edit.set_file_text(file, text));
    let db = fixture.db();
    // The commit made the crate's arena again: its id is taken in the new snapshot.
    let krate = main_crate(db);
    let runs = db.runs().expect("a fixture's database logs its runs");
    _ = runs.take();
    _ = used_imports_with_traits(db, krate);
    // Each run by its query's own name, the last part of its path.
    runs.take()
        .iter()
        .map(|ran| ran.query.rsplit("::").next().unwrap_or(ran.query).to_owned())
        .collect()
}

/// The folds over the crate's uses: what an edit whose uses are equal must not run.
const FOLDS: [&str; 2] = ["crate_uses", "used_imports_with_traits"];

/// The invariant: an edit of a body that changes none of its uses executes no imports fold;
/// only the edited body's own records (`body_uses`, `body_trait_uses`) run again, and come out
/// equal. The control: an edit that adds a type path through the import of `Unused`, which
/// changes the body's uses, runs both folds; a record keyed by the body's text (the planted bug: the fold reading the
/// body, as it did before the per-body records) would run them on the first edit too.
///
/// False by design since the port to blaze-memo (port decision 3: the memo is not the
/// incremental mechanism): it claimed the edited crate's folds are not run again after an edit
/// of one of its bodies. Now the commit drops the edited crate's arena wholesale, and every
/// query of that crate, the folds too, runs again in the new snapshot; what an edit costs
/// within its crate is the item store's and the kernel's to keep small, not the memo's. Left
/// failing until it is restated over the body entries.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn a_body_edit_with_equal_uses_reruns_no_fold() {
    let equal = executed_after(&with_by_method("1u8.method();"));
    assert!(equal.iter().any(|it| it == "body_uses"), "the body's record ran again: {equal:?}");
    for fold in FOLDS {
        assert!(!equal.iter().any(|it| it == fold), "{fold} ran on an equal-uses edit: {equal:?}");
    }
    let changed =
        executed_after(&with_by_method("0u8.method();\n    let _: Option<&dyn Unused> = None;"));
    for fold in FOLDS {
        assert!(changed.iter().any(|it| it == fold), "{fold} did not run: {changed:?}");
    }
}

/// Paths that resolve to nothing, lookups the traits in scope answer, a macro call and its
/// rule, a call of no macro, and a method call type checking cannot resolve.
const RECORDS: &str = r#"
//- /lib.rs crate:main
mod m {
    pub trait Method { fn method(&self) {} }
    impl Method for u8 {}
    pub trait Other { fn missing(&self) {} }
}
use m::Method;
use m::Other;
macro_rules! two { () => {}; (x) => {} }
struct S;
fn f() {
    0u8.method();
    S::missing;
    nowhere::g();
    m::absent();
    two!(x);
    unknown_mac!();
    ().missing();
}
"#;

/// The records the imports lint reads in place of its syntax walk, of `source`.
fn records(source: &str) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>, bool) {
    let db = fixture::db(source).into_db();
    let used = used_imports_with_traits(&db, main_crate(&db));
    let names = used
        .unresolved_names()
        .iter()
        .map(|it| format!("{}:{}", it.name.as_str(), it.lexical))
        .collect();
    let asks = used.trait_asks().iter().map(|it| it.name.as_str().to_owned()).collect();
    let invoked = used.invoked().iter().map(|it| format!("{:?}", it.arm)).collect();
    let unknown = used.macros_unknown().iter().map(|it| it.as_str().to_owned()).collect();
    // `use m::Other` is used only by type checking (the failed `().missing()`), never by a path.
    let other = used
        .imports()
        .map(|(it, _)| it)
        .find(|&it| match it {
            ImportOrExternCrate::Import(ImportId { use_, idx }) => {
                use_tree_to_view(&db, use_.lookup(&db).id, idx).to_string() == "m::Other"
            }
            _ => false,
        })
        .is_some_and(|it| used.typeck_used(it) && used.paths_used(it).is_none());
    (names, asks, invoked, unknown, other)
}

/// The invariant: the front end's per-body records hold each unresolved path's first missing
/// segment (lexical for a plain path's first), each method call's, field's and type-relative
/// path's name for the traits in scope, each macro call's matched rule, each call of no macro's
/// name, and keep type checking's uses of a trait apart from the paths'. Control: the same
/// crate with the failing paths written to resolve records no unresolved name, and with the
/// failing method call removed `m::Other` is not used by type checking.
#[test]
fn the_body_records_hold_what_the_lint_reads() {
    let (names, asks, invoked, unknown, other) = records(RECORDS);
    assert_eq!(names, ["nowhere:true", "absent:false"]);
    assert_eq!(asks, ["method", "missing"]);
    assert_eq!(invoked, ["Some(1)"]);
    assert_eq!(unknown, ["unknown_mac"]);
    assert!(other, "a failed method call's candidate trait is type checking's, not a path's");
    let fixed = RECORDS
        .replace("nowhere::g();", "m::Method::method(&0u8);")
        .replace("m::absent();", "")
        .replace("().missing();", "");
    let (names, _, _, _, other) = records(&fixed);
    assert!(names.is_empty(), "{names:?}");
    assert!(!other, "with no failed call `m::Other` is unused");
}

/// An import used only by a derive's path, one used only by an attribute macro's path on a
/// module item, and one used only by an attribute macro's path on an associated item.
const MACRO_PATHS: &str = r#"
//- minicore: derive, clone, global_allocator
//- /lib.rs crate:main
use core::clone::Clone as Dup;
use core::prelude::v1::global_allocator as ga;
use core::prelude::v1::global_allocator as ga_assoc;
#[derive(Dup)]
struct S;
#[ga]
static A: S = S;
struct T;
impl T {
    #[ga_assoc]
    fn f() {}
}
"#;

/// The imports of `source` a path used, as their trees' text, sorted, and how many macros it
/// invoked with no rule told.
fn macro_path_uses(source: &str) -> (Vec<String>, usize) {
    let db = fixture::db(source).into_db();
    let used = used_imports_with_traits(&db, main_crate(&db));
    let mut texts: Vec<String> = used
        .imports()
        .filter(|&(it, _)| used.paths_used(it).is_some())
        .filter_map(|(import, _)| match import {
            ImportOrExternCrate::Import(ImportId { use_, idx }) => {
                Some(use_tree_to_view(&db, use_.lookup(&db).id, idx).to_string())
            }
            _ => None,
        })
        .collect();
    texts.sort();
    let no_rule = used.invoked().iter().filter(|it| it.arm.is_none()).count();
    (texts, no_rule)
}

/// What decides why an associated item's attribute path records no use, each step of
/// hir-def's `Walk::attr_call` (nameres/uses.rs) redone over `source`'s impls: the impl's
/// macro calls (none: the impl's collection kept no attribute call), each call's kind, the
/// meta `find_attr_range_view` picks and the path made of it, what `resolve_path_as_macro`
/// finds and whether its definition is the call's (`attr_call` returns before recording
/// when not), and the uses `record_macro_path_uses` gives (none with the two equal: the
/// lookup of the path's segments misses the import).
fn assoc_attr_evidence(source: &str) -> String {
    use hir_def::{nameres::MacroSubNs, resolver::HasResolver as _};
    use hir_expand::{MacroCallKind, mod_path::ModPath};
    use syntax::view;

    let db = fixture::db(source).into_db();
    let db = &db;
    let krate = main_crate(db);
    let mut out = String::from("evidence:\n");
    for (_, module) in hir_def::nameres::crate_def_map(db, krate).modules(db) {
        for imp in module.scope(db).impls() {
            let items = imp.impl_items(db);
            out.push_str(&format!(
                "  impl {imp:?}: {} items, macro calls {:?}\n",
                items.items.len(),
                items.macro_calls,
            ));
            let resolver = imp.resolver(db);
            // The impl's own diagnostics (an unresolved attribute macro is one), and what the
            // attribute's name resolves to here: `resolve_attr_macro` (attr_resolution.rs)
            // keeps a call only where `MacroId::is_attribute` holds, the module collector
            // (facts/macros.rs) where `MacroDefId::is_attribute` does.
            out.push_str(&format!(
                "  impl {imp:?} diagnostics: {:?}\n",
                imp.impl_items_with_diagnostics(db).1,
            ));
            let name = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("ga_assoc"));
            let by_name = ModPath::from_segments(hir_expand::mod_path::PathKind::Plain, [name]);
            let found = resolver.resolve_path_as_macro(db, &by_name, Some(MacroSubNs::Attr));
            out.push_str(&format!(
                "  `ga_assoc` here: {:?}, kind {:?}, MacroId::is_attribute {:?}, \
                 MacroDefId::is_attribute {:?}\n",
                found.map(|it| it.def),
                found.map(|it| it.def.definition(db).kind),
                found.map(|it| it.def.is_attribute(db)),
                found.map(|it| it.def.definition(db).is_attribute()),
            ));
            for &(item, call) in items.macro_calls.iter() {
                let loc = call.loc(db);
                let MacroCallKind::Attr { censored_attr_ids, .. } = &loc.kind else {
                    out.push_str(&format!("  call {call:?}: not an attribute call\n"));
                    continue;
                };
                let (_, meta) =
                    censored_attr_ids.invoc_attr().find_attr_range_view(db, loc.krate, item);
                let ast_path = match meta {
                    view::Meta::PathMeta(it) => it.path(),
                    view::Meta::TokenTreeMeta(it) => it.path(),
                    _ => None,
                };
                let meta_text = view::AstNode::syntax(&meta).text().to_string();
                let Some(ast_path) = ast_path else {
                    out.push_str(&format!("  call {call:?}: meta `{meta_text}` has no path\n"));
                    continue;
                };
                let span_map = item.file_id.span_map(db);
                let table = item.file_id.context_arena();
                let path = ModPath::from_src_view(db, table, ast_path, &mut |range| {
                    span_map.span_for_range(range).ctx
                });
                let Some(path) = path else {
                    out.push_str(&format!("  call {call:?}: meta `{meta_text}`, no ModPath\n"));
                    continue;
                };
                let subns = Some(MacroSubNs::Attr);
                let found = resolver.resolve_path_as_macro(db, &path, subns);
                let found_def = found.map(|it| it.def.definition(db));
                let mut uses = Vec::new_in(std::alloc::Global);
                resolver.record_macro_path_uses(db, &path, subns, &mut uses);
                out.push_str(&format!(
                    "  call {call:?}: meta `{meta_text}`, path {:?} {:?}, table {table:?}, \
                     resolver table {:?}\n    found {:?}, its definition is the call's: {}\n    \
                     found def {found_def:?}\n    call def {:?}\n    uses {uses:?}\n",
                    path.kind,
                    path.segments(),
                    resolver.ctx_table(),
                    found.map(|it| it.def),
                    found_def == Some(loc.def),
                    loc.def,
                ));
            }
        }
    }
    out
}

/// The invariant: the path of each derive and attribute macro is resolved again as a use, so an
/// import only `#[derive(Dup)]`, a module item's `#[ga]` or an associated item's `#[ga_assoc]`
/// reads is used by a path, and each such call is recorded invoked with no rule. Control: with
/// the three attributes removed none of the three imports is used (the planted bug, the
/// collector recording only `bang: true` paths and the walk only function-like calls, gives that
/// answer for the full fixture too).
#[test]
fn derive_and_attribute_macro_paths_are_uses() {
    let (texts, no_rule) = macro_path_uses(MACRO_PATHS);
    // The evidence is made only on a failure (an `assert_eq!`'s message is).
    assert_eq!(
        texts,
        [
            "core::clone::Clone as Dup",
            "core::prelude::v1::global_allocator as ga",
            "core::prelude::v1::global_allocator as ga_assoc",
        ],
        "{}",
        assoc_attr_evidence(MACRO_PATHS),
    );
    // `Dup`'s derive and `global_allocator` (both attributes name it, one record) at least.
    assert!(no_rule >= 2, "the derive and the attribute are invoked with no rule: {no_rule}");
    let bare = MACRO_PATHS
        .replace("#[derive(Dup)]\n", "")
        .replace("#[ga]\n", "")
        .replace("    #[ga_assoc]\n", "");
    let (texts, _) = macro_path_uses(&bare);
    assert!(texts.is_empty(), "{texts:?}");
}

/// A trait in scope through two imports, a method call of it type checking cannot resolve.
const TWO_IMPORTS: &str = r#"
//- /lib.rs crate:main
mod m {
    pub trait Other { fn missing(&self) {} }
}
use m::Other;
use m::Other as Again;
fn f() {
    ().missing();
}
"#;

/// The invariant: a method call type checking resolves to nothing counts every import that
/// brings a trait with a value item of its name into scope as used by type checking, as rustc
/// may pick any of them, not only the first `record_trait_uses` gives. Control: the planted
/// bug, one binding per trait, leaves `m::Other as Again` out, which the count of two catches;
/// with the call removed neither is used.
#[test]
fn a_failed_method_call_uses_every_import_of_its_traits() {
    let typeck = |source: &str| {
        let db = fixture::db(source).into_db();
        let used = used_imports_with_traits(&db, main_crate(&db));
        let mut texts: Vec<String> = used
            .imports()
            .filter(|&(it, _)| used.typeck_used(it))
            .filter_map(|(import, _)| match import {
                ImportOrExternCrate::Import(ImportId { use_, idx }) => {
                    Some(use_tree_to_view(&db, use_.lookup(&db).id, idx).to_string())
                }
                _ => None,
            })
            .collect();
        texts.sort();
        texts
    };
    assert_eq!(typeck(TWO_IMPORTS), ["m::Other", "m::Other as Again"]);
    assert!(typeck(&TWO_IMPORTS.replace("().missing();", "")).is_empty());
}

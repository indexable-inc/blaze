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
    fixture.db_mut().set_file_text(file, text);
    let db = fixture.db();
    db.log_executed(|| _ = used_imports_with_traits(db, krate))
}

/// The folds over the crate's uses: what an edit whose uses are equal must not run.
const FOLDS: [&str; 2] = ["crate_uses", "used_imports_with_traits"];

/// The invariant: an edit of a body that changes none of its uses executes no imports fold;
/// only the edited body's own records (`body_uses`, `body_trait_uses`) run again, and come out
/// equal. The control: an edit that adds a type path through the import of `Unused`, which
/// changes the body's uses, runs both folds; a record keyed by the body's text (the planted bug: the fold reading the
/// body, as it did before the per-body records) would run them on the first edit too.
#[test]
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

/// The invariant: the path of each derive and attribute macro is resolved again as a use, so an
/// import only `#[derive(Dup)]`, a module item's `#[ga]` or an associated item's `#[ga_assoc]`
/// reads is used by a path, and each such call is recorded invoked with no rule. Control: with
/// the three attributes removed none of the three imports is used (the planted bug, the
/// collector recording only `bang: true` paths and the walk only function-like calls, gives that
/// answer for the full fixture too).
#[test]
fn derive_and_attribute_macro_paths_are_uses() {
    let (texts, no_rule) = macro_path_uses(MACRO_PATHS);
    assert_eq!(
        texts,
        [
            "core::clone::Clone as Dup",
            "core::prelude::v1::global_allocator as ga",
            "core::prelude::v1::global_allocator as ga_assoc",
        ]
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

use base_db::{CrateGraphBuilder, CrateName, DependencyBuilder, all_crates};
use expect_test::{Expect, expect};
use hir_expand::fixture;

use crate::{
    file_item_tree,
    nameres::{
        crate_def_map,
        tests::{TestDB, TestDBExt},
    },
};

fn check_def_map_is_not_recomputed(
    #[rust_analyzer::rust_fixture] ra_fixture_initial: &str,
    #[rust_analyzer::rust_fixture] ra_fixture_change: &str,
    expecta: &Expect,
    expectb: &Expect,
) {
    let fx = fixture::db(ra_fixture_initial);
    let pos = fx.position();
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    // The fixture's write resolved the crate: the first read takes what the maintainer
    // published.
    execute_assert_events(
        &db,
        || {
            crate_def_map(&db, krate);
        },
        &[],
        expecta,
    );
    let file_id = pos.file_id.file_id(&db);
    execute_assert_write(
        &mut db,
        |db| {
            db.set_file_text(file_id, ra_fixture_change);
            crate_def_map(db, krate);
        },
        &[("crate_local_def_map", 0)],
        expectb,
    );
}

#[test]
fn crate_metadata_changes_should_not_invalidate_unrelated_def_maps() {
    let fx = fixture::db(
        r#"
//- /a.rs crate:a
pub fn foo() {}

//- /b.rs crate:b
pub struct Bar;

//- /c.rs crate:c deps:b
pub const BAZ: u32 = 0;
    "#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();

    for &krate in all_crates(&db).iter() {
        crate_def_map(&db, krate);
    }

    let all_crates_before = all_crates(&db);
    let a = *all_crates_before
        .iter()
        .find(|it| {
            it.extra_data(&db).display_name.as_ref().is_some_and(|name| name.to_string() == "a")
        })
        .expect("crate a");
    let a_before = crate::nameres::delta::Slices::of(&db, a);

    let new_crate_graph = {
        // Add dependencies: c -> b, b -> a.
        let mut new_crate_graph = CrateGraphBuilder::default();

        // Each crate as the fixture made it (its cfg, crate attributes and the rest), only its
        // dependencies new: what changes is the graph, not the crates.
        let mut add_crate = |root_file_idx: usize| {
            let root = files[root_file_idx].file_id(&db);
            let old = *all_crates_before
                .iter()
                .find(|it| it.data(&db).root_file_id == root)
                .expect("a crate of the fixture's root file");
            let (data, extra) = (old.data(&db), old.extra_data(&db));
            let attrs = data
                .crate_attrs
                .iter()
                .map(|it| {
                    it.strip_prefix("#![")
                        .and_then(|it| it.strip_suffix(']'))
                        .unwrap_or(it)
                        .to_owned()
                })
                .collect();
            new_crate_graph.add_crate_root(
                root,
                data.edition,
                extra.display_name.clone(),
                extra.version.clone(),
                old.cfg_options(&db).clone(),
                extra.potential_cfg_options.clone(),
                old.env(&db).clone(),
                data.origin.clone(),
                attrs,
                data.target_kind,
                data.proc_macro_cwd.clone(),
                old.workspace_data(&db).clone(),
            )
        };
        let a = add_crate(0);
        let b = add_crate(1);
        let c = add_crate(2);
        new_crate_graph
            .add_dep(c, DependencyBuilder::new(CrateName::new("b").unwrap(), b))
            .unwrap();
        new_crate_graph
            .add_dep(b, DependencyBuilder::new(CrateName::new("a").unwrap(), a))
            .unwrap();
        new_crate_graph
    };

    let events = db.log_write_in_read_order(|db| {
        new_crate_graph.set_in_db(db);
        for &krate in all_crates(db).iter() {
            crate_def_map(db, krate);
        }
    });
    let a_after = crate::nameres::delta::Slices::of(&db, a);
    assert_eq!(
        a_before.differing(&a_after),
        Vec::<String>::new(),
        "fields of crate a's resolution set"
    );
    // `b` gets invalidated due to its new dependency edge to `a`. `c` already depended on `b`,
    // and `b` declares what it did, so `c`'s def map stays as it was.
    let names: Vec<(blaze_query::db::Id, String)> = all_crates(&db)
        .iter()
        .map(|&krate| {
            let name = krate.extra_data(&db).display_name.as_ref().map(|it| it.to_string());
            (blaze_query::db::AsId::as_id(&krate), name.unwrap_or_default())
        })
        .collect();
    let storage = blaze_query::db::Database::storage(&db);
    let mut rerun: Vec<&str> = events
        .iter()
        .filter_map(|event| match event.kind {
            blaze_query::db::EventKind::WillExecute { database_key }
                if storage.ingredient_name(database_key.ingredient_index())
                    == "crate_local_def_map" =>
            {
                let key = database_key.key_index();
                Some(names.iter().find(|(id, _)| *id == key).map_or("?", |(_, name)| name.as_str()))
            }
            _ => None,
        })
        .collect();
    rerun.sort_unstable();
    assert_eq!(rerun, ["b"], "the crates whose def map was collected again");
    let all_crates_after = all_crates(&db);
    assert!(
        std::sync::Arc::ptr_eq(&all_crates_before, &all_crates_after),
        "the all_crates list should not have been invalidated"
    );
}

#[test]
fn typing_inside_a_function_should_not_invalidate_def_map() {
    check_def_map_is_not_recomputed(
        r"
//- /lib.rs
mod foo;$0

use crate::foo::bar::Baz;

enum E { A, B }
use E::*;

fn foo() -> i32 {
    1 + 1
}

#[cfg(never)]
fn no() {}
//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
pub struct Baz;
",
        r"
mod foo;

use crate::foo::bar::Baz;

enum E { A, B }
use E::*;

fn foo() -> i32 { 92 }

#[cfg(never)]
fn no() {}
",
        &expect![[r#"
            [
                "crate_local_def_map",
                "resolution_of",
            ]
        "#]],
        // `use E::*` binds `E`'s variants, which no round-0 log holds: the maintainer reads
        // them again (`variants_syntax`, which reads the enum's `signature_syntax`) to see
        // whether the glob must run again.
        &expect![[r#"
            [
                "EditionedFileId::parse_",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "real_span_map",
                "file_outline",
                "signature_syntax",
            ]
        "#]],
    );
}

#[test]
fn typing_inside_a_macro_should_not_invalidate_def_map() {
    check_def_map_is_not_recomputed(
        r"
//- /lib.rs
macro_rules! m {
    ($ident:ident) => {
        fn f() {
            $ident + $ident;
        };
    }
}
mod foo;

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
$0
m!(X);

pub struct S {}
",
        r"
m!(Y);

pub struct S {}
",
        &expect![[r#"
            [
                "crate_local_def_map",
                "resolution_of",
            ]
        "#]],
        &expect![[r#"
            [
                "EditionedFileId::parse_",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "real_span_map",
                "MacroCallId::macro_arg",
                "MacroCallId::parse_macro_expansion",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "expanded_item_key",
            ]
        "#]],
    );
}

#[test]
fn typing_inside_an_attribute_should_not_invalidate_def_map() {
    check_def_map_is_not_recomputed(
        r"
//- proc_macros: identity
//- /lib.rs
mod foo;

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
$0
#[proc_macros::identity]
fn f() {}
",
        r"
#[proc_macros::identity]
fn f() { foo }
",
        &expect![[r#"
            [
                "crate_local_def_map",
                "resolution_of",
            ]
        "#]],
        &expect![[r#"
            [
                "resolution_of",
                "EditionedFileId::parse_",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "real_span_map",
                "deferred_item_shape",
                "MacroCallId::macro_arg",
                "EditionedFileId::parse_errors",
                "MacroCallId::parse_macro_expansion",
                "MacroCallId::proc_macro_expansion",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "expanded_item_key",
            ]
        "#]],
    );
}

// Would be nice if this was the case, but as attribute inputs are stored in the item tree, this is
// not currently the case.
// #[test]
// fn typing_inside_an_attribute_arg_should_not_invalidate_def_map() {
//     check_def_map_is_not_recomputed(
//         r"
// //- proc_macros: identity
// //- /lib.rs
// mod foo;

// //- /foo/mod.rs
// pub mod bar;

// //- /foo/bar.rs
// $0
// #[proc_macros::identity]
// fn f() {}
// ",
//         r"
// #[proc_macros::identity(foo)]
// fn f() {}
// ",
//     );
// }

#[test]
fn typing_inside_macro_heavy_file_should_not_invalidate_def_map() {
    check_def_map_is_not_recomputed(
        r"
//- proc_macros: identity, derive_identity
//- /lib.rs
macro_rules! m {
    ($ident:ident) => {
        fn fm() {
            $ident + $ident;
        };
    }
}
mod foo;

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
$0
fn f() {}

m!(X);
macro_rules! m2 {
    ($ident:ident) => {
        fn f2() {
            $ident + $ident;
        };
    }
}
m2!(X);

#[proc_macros::identity]
#[derive(proc_macros::DeriveIdentity)]
pub struct S {}
",
        r"
fn f() {0}

m!(X);
macro_rules! m2 {
    ($ident:ident) => {
        fn f2() {
            $ident + $ident;
        };
    }
}
m2!(X);

#[proc_macros::identity]
#[derive(proc_macros::DeriveIdentity)]
pub struct S {}
",
        &expect![[r#"
            [
                "crate_local_def_map",
                "resolution_of",
            ]
        "#]],
        &expect![[r#"
            [
                "resolution_of",
                "EditionedFileId::parse_",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "real_span_map",
                "MacroCallId::macro_arg",
                "AstId::decl_macro_expander",
                "MacroCallId::macro_arg",
                "deferred_item_shape",
                "MacroCallId::macro_arg",
                "EditionedFileId::parse_errors",
            ]
        "#]],
    );
}

// Would be nice if this was the case, but as attribute inputs are stored in the item tree, this is
// not currently the case.
// #[test]
// fn typing_inside_a_derive_should_not_invalidate_def_map() {
//     check_def_map_is_not_recomputed(
//         r"
// //- proc_macros: derive_identity
// //- minicore:derive
// //- /lib.rs
// mod foo;

// //- /foo/mod.rs
// pub mod bar;

// //- /foo/bar.rs
// $0
// #[derive(proc_macros::DeriveIdentity)]
// #[allow()]
// struct S;
// ",
//         r"
// #[derive(proc_macros::DeriveIdentity)]
// #[allow(dead_code)]
// struct S;
// ",
//     );
// }

#[test]
fn typing_inside_a_function_should_not_invalidate_item_expansions() {
    let fx = fixture::db(
        r#"
//- /lib.rs
macro_rules! m {
    ($ident:ident) => {
        fn $ident() { };
    }
}
mod foo;

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
m!(X);
fn quux() { 1$0 }
m!(Y);
m!(Z);
"#,
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    let krate = fixture::test_crate(&db);
    execute_assert_events(
        &db,
        || {
            let crate_def_map = crate_def_map(&db, krate);
            let module_data = &crate_def_map.module(
                &db,
                crate_def_map.modules_for_file(&db, pos.file_id.file_id(&db)).next().unwrap(),
            );
            assert_eq!(module_data.scope(&db).resolutions().count(), 4);
        },
        // The fixture's write made the six item trees and the three expansions; the module is
        // read from the published resolution for the first time.
        &[("file_item_tree_query", 0), ("parse_macro_expansion", 0)],
        &expect![[r#"
            [
                "crate_local_def_map",
                "resolution_of",
                "CrateResolution::module",
            ]
        "#]],
    );

    let new_text = r#"
m!(X);
fn quux() { 92 }
m!(Y);
m!(Z);
"#;
    let file_id = pos.file_id.file_id(&db);
    execute_assert_write(
        &mut db,
        |db| {
            db.set_file_text(file_id, new_text);
            let db: &TestDB = db;
            let crate_def_map = crate_def_map(db, krate);
            let module_data = &crate_def_map
                .module(db, crate_def_map.modules_for_file(db, file_id).next().unwrap());
            assert_eq!(module_data.scope(db).resolutions().count(), 4);
        },
        &[("file_item_tree_query", 1), ("parse_macro_expansion", 0)],
        &expect![[r#"
            [
                "EditionedFileId::parse_",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "real_span_map",
                "MacroCallId::macro_arg",
                "MacroCallId::macro_arg",
                "MacroCallId::macro_arg",
            ]
        "#]],
    );
}

#[test]
fn item_tree_prevents_reparsing() {
    let fx = fixture::db(
        r#"
pub struct S;
pub union U {}
pub enum E {
    Variant,
}
pub fn f(_: S) { $0 }
pub trait Tr {}
impl Tr for () {}
pub const C: u8 = 0;
pub static ST: u8 = 0;
pub type Ty = ();
"#,
    );
    let pos = fx.position();
    let mut db = fx.into_db();

    execute_assert_events(
        &db,
        || {
            file_item_tree(&db, pos.file_id.into(), fixture::test_crate(&db));
        },
        // The fixture's write made the tree, resolving the crate.
        &[("file_item_tree_query", 0), ("parse", 0)],
        &expect![[r#"
            []
        "#]],
    );

    let file_id = pos.file_id.file_id(&db);
    let file_text = format!("{}\n", db.file_text(file_id).text(&db));
    execute_assert_write(
        &mut db,
        |db| {
            db.set_file_text(file_id, &file_text);
            let db: &TestDB = db;
            file_item_tree(db, pos.file_id.into(), fixture::test_crate(db));
        },
        &[("file_item_tree_query", 1), ("parse", 1)],
        &expect![[r#"
            [
                "EditionedFileId::parse_",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "real_span_map",
            ]
        "#]],
    );
}

fn execute_assert_events(
    db: &TestDB,
    f: impl FnOnce(),
    required: &[(&str, usize)],
    expect: &Expect,
) {
    let events = db.executed_in_read_order(f);
    assert_events(&events, required, expect);
}

/// [`execute_assert_events`] of a write and what reads after it: every write ends in the
/// nameres maintainer, which runs in it what a read once ran first.
fn execute_assert_write(
    db: &mut TestDB,
    f: impl FnOnce(&mut TestDB),
    required: &[(&str, usize)],
    expect: &Expect,
) {
    let events = db.executed_in_write(f);
    assert_events(&events, required, expect);
}

fn assert_events(events: &[String], required: &[(&str, usize)], expect: &Expect) {
    for (event, count) in required {
        let n = events.iter().filter(|it| it.contains(event)).count();
        assert_eq!(
            n, *count,
            "Expected {event} to be executed {count} times, but got {n}: {events:#?}"
        );
    }
    expect.assert_debug_eq(&events);
}

#[test]
fn item_after_impl_is_its_own_span_anchor() {
    use hir_expand::HirFileId;
    use syntax::{
        ast as tag,
        view::{self as ast, AstNode, HasModuleItem},
    };

    let fx = fixture::db(
        r#"
struct S;
impl S {
    fn first(&self) {}
    fn last(&self) {$0}
}
struct AfterImpl;
"#,
    );
    let pos = fx.position();
    let db = fx.into_db();
    let file_id = HirFileId::from(pos.file_id);
    let after_impl = pos.file_id.parse(&db).view().items().last().unwrap();
    assert!(matches!(after_impl, ast::Item::Struct(_)));
    let span = file_id.span_map(&db).span_for_range(after_impl.syntax().text_range());

    assert_eq!(
        span.anchor.ast_id,
        file_id.ast_id_map(&db).ast_id_of_view::<tag::Item>(after_impl).erase()
    );
    assert_eq!(u32::from(span.range.start()), 0);
}

/// An attribute that gives back its item and counts its runs, each call stating `state` of its
/// session, its calls ordered as those of a crate that keeps a session when `ordered`.
#[derive(Debug)]
struct Counting {
    runs: std::sync::atomic::AtomicUsize,
    ordered: bool,
    state: hir_expand::proc_macro::StateUse,
}

impl hir_expand::proc_macro::ProcMacroExpander for Counting {
    fn requires_session(&self) -> bool {
        self.ordered
    }

    fn expand(
        &self,
        _: &base_db::Db,
        call: hir_expand::proc_macro::ProcMacroCall<'_>,
    ) -> Result<hir_expand::proc_macro::Expanded, hir_expand::proc_macro::ProcMacroExpansionError>
    {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(hir_expand::proc_macro::Expanded {
            tree: call.subtree.copy(),
            reads: hir_expand::proc_macro::ExpansionReads::Nothing,
            state: self.state,
        })
    }

    // dyn: `ProcMacroExpander::eq_dyn`'s signature, compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn hir_expand::proc_macro::ProcMacroExpander) -> bool {
        std::ptr::eq(self as *const Self as *const (), other as *const _ as *const ())
    }
}

/// What resolving a crate with one call of [`Counting`] does, and an edit of a file the call
/// does not read: its runs when resolved, its runs after the edit, whether the def map says it
/// used a call whose session took part, and how many expansions the edit's write ran.
fn counting_across_an_unrelated_edit(
    ordered: bool,
    state: hir_expand::proc_macro::StateUse,
) -> (usize, usize, bool, usize) {
    let counting = std::sync::Arc::new(Counting { runs: Default::default(), ordered, state });
    let extra = vec![(
        "#[proc_macro_attribute]\npub fn counting(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n"
            .to_owned(),
        hir_expand::proc_macro::ProcMacro {
            name: intern::Symbol::intern("counting"),
            kind: hir_expand::proc_macro::ProcMacroKind::Attr,
            expander: counting.clone(),
            disabled: false,
        },
    )];
    let fx = fixture::db_with_proc_macros(
        r#"
//- proc_macros: counting
//- /lib.rs
mod other;
#[proc_macros::counting]
pub struct Host;
//- /other.rs
pub fn unrelated() -> u32 { 1 }
"#,
        extra,
    );
    let other = fx.file_at("/other.rs");
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    let marked = {
        let def_map = crate_def_map(&db, krate);
        let dump = def_map.dump(&db);
        assert!(dump.contains("Host"), "the call's item is in the def map:\n{dump}");
        def_map.session_macros
    };
    let resolved = counting.runs.load(std::sync::atomic::Ordering::Relaxed);
    let file = other.file_id(&db);
    let events = db.executed_in_write(|db| {
        db.set_file_text(file, "pub fn unrelated() -> u32 { 2 }\n");
        crate_def_map(db, krate);
    });
    let expanded = events.iter().filter(|it| it.contains("proc_macro_expansion")).count();
    (resolved, counting.runs.load(std::sync::atomic::Ordering::Relaxed), marked, expanded)
}

/// A call whose output is fixed by its call and reads runs once and is reused across an edit
/// of another file, whether its crate keeps no state or settles (its calls ordered, the call
/// shown to give the same from either state): its def map is not marked as using the session,
/// so nothing about it reruns each revision. A call whose session's state takes part marks it.
#[test]
fn a_fixed_expansion_is_reused_across_an_unrelated_edit() {
    use hir_expand::proc_macro::StateUse;
    for ordered in [false, true] {
        let (resolved, edited, marked, expanded) =
            counting_across_an_unrelated_edit(ordered, StateUse::Fixed);
        assert_eq!(resolved, 1, "ordered {ordered}: the call ran once to resolve the crate");
        assert_eq!((edited, expanded), (1, 0), "ordered {ordered}: the edit ran the call again");
        assert!(!marked, "ordered {ordered}: a fixed call marked the def map as using the session");
    }
    let (resolved, _, marked, _) = counting_across_an_unrelated_edit(
        true,
        StateUse::Session("a test's call that says its session took part"),
    );
    assert_eq!(resolved, 1, "the call ran once to resolve the crate");
    assert!(marked, "a call whose session took part marks the def map");
}

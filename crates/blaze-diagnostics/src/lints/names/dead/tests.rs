//! Allocation budgets of the dead-code pass's hot paths (once the crate is numbered and a
//! definition's level and attributes have been asked for, every answer the walk asks again is
//! a read, and allocates nothing), and the refinements of the numbering and the marks against
//! the walks over syntax they replace, with the edit loop's: a body edit that leaves its marks
//! equal walks that body alone again.

use std::sync::atomic::{AtomicBool, Ordering};

use base_db::Db;
use blaze_alloc_count::assert_at_most;
use hir_def::{
    ExpressionStoreOwnerId,
    expr_store::{Body, ExpressionStore},
};

use super::{
    defs::{DefIdx, DefSet, Defs, Id, Order, Parent, reference},
    level::Levels,
    mark::{self, How, StoreMark, store_marks},
    reach::Reach,
    report,
    seed::{self, Crate},
};
use crate::{
    Options,
    cx::Cx,
    lints::names::effective::Effective,
    tests::{check_all, fixture_db},
};

const FIXTURE: &str = r#"
//- /main.rs crate:main
trait T {
    fn f(&self);
    type A;
}
enum E { A(i32), B }
struct S(i32);
impl T for S {
    fn f(&self) {}
    type A = E;
}
impl S {
    const C: i32 = 1;
}
#[allow(dead_code)]
fn unused() {}
fn main() {
    S(1).f();
}
"#;

/// `check` given the numbered crate of the fixture's first workspace crate.
fn with_crate(check: impl FnOnce(&Crate<'_, '_, '_>)) {
    let checked = crate::tests::checked(FIXTURE);
    let db = &checked.db;
    blaze_query::db::attach(db, || {
        let krate = hir::check::workspace_crates(db)[0];
        let frame = crate::tests::frame();
        let cx = Cx::new(db, krate, checked.files(), &frame);
        let defs = Defs::of(&cx);
        let levels = Levels::new();
        let effective = Effective::of(&cx);
        let reach = Reach::of(&cx, &defs, &effective);
        let k = Crate::new(&cx, &defs, &levels, &reach);
        check(&k);
    });
}

/// A definition's level and `allow_or_lang`, asked a second time, are a `Cell` read.
#[test]
fn cached_level_and_attributes() {
    with_crate(|k| {
        for def in k.defs.all() {
            k.level(def);
            k.allow_or_lang(def);
        }
        assert_at_most(0, "a cached level or attribute answer", || {
            for def in k.defs.all() {
                k.level(def);
                k.allow_or_lang(def);
            }
        });
    });
}

/// Looking a definition up by its id, its children, its impl and its trait's item reads the
/// numbering.
#[test]
fn numbering_lookups() {
    with_crate(|k| {
        let defs = k.defs;
        assert_ne!(defs.len(), 0, "the fixture has definitions");
        assert_at_most(0, "a lookup in the crate's numbering", || {
            for def in defs.all() {
                let _ = defs.get(defs[def].id);
                let _ = defs.children(def);
                let _ = defs.impl_of(def);
                let _ = defs.trait_item_of(def);
                let _ = k.reach.is_reachable(def);
            }
        });
    });
}

/// A bitset over the definitions tests and inserts in place.
#[test]
fn bitset_test_and_insert() {
    let mut bits = DefSet::new(1000);
    assert_at_most(0, "a bitset's test and insert", || {
        for at in 0..1000 {
            assert!(!bits.contains(at));
            assert!(bits.insert(at));
            assert!(!bits.insert(at));
        }
    });
}

/// The sources of the pass's UI tests (`src/tests/lints/names/dead/`), by name.
const UI: &[(&str, &str)] = &[
    ("allow-trait-or-impl", include_str!("../../../tests/lints/names/dead/allow-trait-or-impl.rs")),
    ("basic", include_str!("../../../tests/lints/names/dead/basic.rs")),
    ("const-underscore-issue-101532", include_str!("../../../tests/lints/names/dead/const-underscore-issue-101532.rs")),
    ("empty-unused-enum", include_str!("../../../tests/lints/names/dead/empty-unused-enum.rs")),
    ("enum-variants", include_str!("../../../tests/lints/names/dead/enum-variants.rs")),
    ("impl-trait", include_str!("../../../tests/lints/names/dead/impl-trait.rs")),
    ("in-closure", include_str!("../../../tests/lints/names/dead/in-closure.rs")),
    ("issue-41883", include_str!("../../../tests/lints/names/dead/issue-41883.rs")),
    ("issue-68408-false-positive", include_str!("../../../tests/lints/names/dead/issue-68408-false-positive.rs")),
    ("issue-85255", include_str!("../../../tests/lints/names/dead/issue-85255.rs")),
    ("leading-underscore", include_str!("../../../tests/lints/names/dead/leading-underscore.rs")),
    ("lint-dead-code-1", include_str!("../../../tests/lints/names/dead/lint-dead-code-1.rs")),
    ("lint-dead-code-2", include_str!("../../../tests/lints/names/dead/lint-dead-code-2.rs")),
    ("lint-dead-code-3", include_str!("../../../tests/lints/names/dead/lint-dead-code-3.rs")),
    ("lint-dead-code-4", include_str!("../../../tests/lints/names/dead/lint-dead-code-4.rs")),
    ("lint-dead-code-5", include_str!("../../../tests/lints/names/dead/lint-dead-code-5.rs")),
    ("lint-dead-code-6", include_str!("../../../tests/lints/names/dead/lint-dead-code-6.rs")),
    ("lint-unused-adt-appeared-in-pattern", include_str!("../../../tests/lints/names/dead/lint-unused-adt-appeared-in-pattern.rs")),
    ("multiple-dead-codes-in-the-same-struct", include_str!("../../../tests/lints/names/dead/multiple-dead-codes-in-the-same-struct.rs")),
    ("pub-field-in-priv-mod", include_str!("../../../tests/lints/names/dead/pub-field-in-priv-mod.rs")),
    ("trait-impl", include_str!("../../../tests/lints/names/dead/trait-impl.rs")),
    ("tuple-struct-field", include_str!("../../../tests/lints/names/dead/tuple-struct-field.rs")),
    ("type-alias", include_str!("../../../tests/lints/names/dead/type-alias.rs")),
    ("unused-adt-impl-pub-trait-with-assoc-const", include_str!("../../../tests/lints/names/dead/unused-adt-impl-pub-trait-with-assoc-const.rs")),
    ("unused-adt-impls-pub-trait", include_str!("../../../tests/lints/names/dead/unused-adt-impls-pub-trait.rs")),
    ("unused-adt-impls-trait", include_str!("../../../tests/lints/names/dead/unused-adt-impls-trait.rs")),
    ("unused-assoc-fns", include_str!("../../../tests/lints/names/dead/unused-assoc-fns.rs")),
    ("unused-enum", include_str!("../../../tests/lints/names/dead/unused-enum.rs")),
    ("unused-struct-derive-default", include_str!("../../../tests/lints/names/dead/unused-struct-derive-default.rs")),
    ("unused-struct-variant", include_str!("../../../tests/lints/names/dead/unused-struct-variant.rs")),
    ("unused-trait-with-assoc-const", include_str!("../../../tests/lints/names/dead/unused-trait-with-assoc-const.rs")),
    ("unused-trait-with-assoc-ty", include_str!("../../../tests/lints/names/dead/unused-trait-with-assoc-ty.rs")),
    ("unused-variant-pub", include_str!("../../../tests/lints/names/dead/unused-variant-pub.rs")),
    ("unused-variant", include_str!("../../../tests/lints/names/dead/unused-variant.rs")),
    ("with-impl", include_str!("../../../tests/lints/names/dead/with-impl.rs")),
];

/// The shapes the UI tests have few of: items in bodies (in blocks, closures, `async` and
/// `const` blocks, array lengths, discriminants), macro calls among items and statements, an
/// inline module, extern blocks, the items of traits and impls with items in their bodies,
/// builtin derives before later items, `use`s, a private `macro`.
const SHAPES: &str = r#"
macro_rules! items { ($($i:item)*) => { $($i)* }; }
items! { fn from_macro() { fn in_macro_body() {} } struct FromMacro; }
#[derive(Clone, Copy)]
struct Derived(u8);
fn after_derived() {}
mod inline {
    pub use super::Derived;
    pub(crate) fn inner() { fn nested() {} }
    extern "C" { fn foreign(); static FOREIGN: u8; }
}
enum Discr { A = { fn in_discr() {} 1 }, B }
trait Tr { fn provided(&self) { fn in_provided() {} } type Ty; const C: u8 = { struct InConst; 0 }; }
impl Tr for Derived { type Ty = u8; fn provided(&self) { fn in_impl() {} } }
macro m() {}
fn bodies() {
    use self::inline::inner;
    fn first() {}
    let _ = { fn in_block() {} };
    let c = || { fn in_closure() {} };
    let _ = async { fn in_async() {} };
    let _ = const { fn in_const_block() {} 0 };
    let _ = [0u8; { fn in_len() {} 2 }];
    items! { fn stmt_macro() {} }
    extern "C" { fn foreign_in_body(); }
    #[derive(Clone)]
    struct InBody { field: u8 }
    impl InBody { fn method(&self) { fn in_method() {} } }
    mod in_body { pub fn never_defined() {} }
    fn last() {}
    c();
}
fn main() {}
"#;

/// The shapes of the marks the UI tests have none of: self-assignments (of a local and a field,
/// in a body and in a signature's anonymous constant), a `use` that re-exports, a glob of an
/// enum, an underscore import, and items in a signature's anonymous constants (a const
/// parameter's default, an array length in a return type and in a field's type).
const MARKS: &str = r#"
mod m {
    pub enum E { A, B }
    pub trait Tr { fn t(&self) {} }
    pub struct Unnamed;
    pub fn reexported() {}
}
pub use m::reexported;
use m::E::*;
use m::Tr as _;
struct Point { x: i32, y: i32 }
fn self_assigns() {
    let mut a = 1;
    a = a;
    let mut p = Point { x: 1, y: 2 };
    p.x = p.x;
    let _ = A;
}
struct WithDefault<const N: usize = { fn in_default() {} 1 }>([u8; N]);
fn in_sig() -> [u8; { fn in_ret_len() {} 2 }] { [0; 2] }
fn assign_in_sig() -> [u8; { let mut z = 1usize; z = z; z }] { [0; 1] }
struct Fields { f: [u8; { fn in_field_len() {} 3 }] }
fn main() {
    self_assigns();
    let _ = in_sig();
    let _ = assign_in_sig();
    let _: Option<Fields> = None;
    let _: Option<WithDefault> = None;
}
"#;

/// The numbering of `cx`'s crate as rows of text: each definition's module, id, kind, name,
/// parent and whether it is foreign, then each module's lists, the owners, every definition's
/// children, the trivial derives and whether the pass is unported. A `use`, and a closure or
/// an anonymous constant, by its node: what both numberings can name.
fn rows(cx: &Cx<'_, '_>, defs: &Defs) -> Vec<String> {
    let db = cx.db;
    let mut rows = Vec::new();
    for def in defs.all() {
        let d = &defs[def];
        let id = match d.id {
            Id::Use(_) => format!("Use({:?})", defs.node(cx, def).expect("a `use` is written")),
            id => format!("{id:?}"),
        };
        let parent = match d.parent {
            Parent::Anon((owner, expr)) => {
                let (_, map) = ExpressionStore::with_source_map(db, owner);
                let Ok(source) = map.expr_syntax(expr) else {
                    panic!("{owner:?}: the anonymous definition {expr:?} is not written")
                };
                format!("Anon({:?})", source.map(|it| it.syntax_node_ptr()))
            }
            parent => format!("{parent:?}"),
        };
        let at = defs.module_of[def as usize];
        rows.push(format!("{def} in {at}: {id} {:?} {:?} {parent} foreign={}", d.kind, d.name, d.foreign));
    }
    let children = defs.all().map(|def| defs.children(def).to_vec());
    let mut trivial: Vec<_> = defs.trivial_derives.iter().map(|(&d, &n)| (d, n)).collect();
    trivial.sort_unstable();
    lists(&mut rows, &defs.modules, &defs.owners, children, &trivial, defs.unported);
    rows
}

/// [`rows`] of the syntax walk's numbering.
fn reference_rows(numbering: &reference::Numbering) -> Vec<String> {
    let mut rows = Vec::new();
    for (def, d) in numbering.defs.iter().enumerate() {
        let at = numbering.module_of[def];
        rows.push(format!(
            "{def} in {at}: {:?} {:?} {:?} {:?} foreign={}",
            d.id, d.kind, d.name, d.parent, d.foreign
        ));
    }
    let len = numbering.defs.len() as DefIdx;
    let edges = &numbering.edges;
    let children =
        (0..len).map(|def| edges.iter().filter(|it| it.0 == def).map(|it| it.1).collect::<Vec<_>>());
    let mut trivial: Vec<_> = numbering.trivial_derives.iter().map(|(&d, &n)| (d, n)).collect();
    trivial.sort_unstable();
    lists(&mut rows, &numbering.modules, &numbering.owners, children, &trivial, numbering.unported);
    rows
}

fn lists(
    rows: &mut Vec<String>,
    modules: &[super::defs::ModuleItems],
    owners: &[DefIdx],
    children: impl Iterator<Item = Vec<DefIdx>>,
    trivial: &[(DefIdx, &str)],
    unported: bool,
) {
    for (at, module) in modules.iter().enumerate() {
        rows.push(format!("module {at}: free {:?} foreign {:?}", module.free, module.foreign));
    }
    rows.push(format!("owners {owners:?}"));
    for (def, children) in children.enumerate() {
        if !children.is_empty() {
            rows.push(format!("children of {def}: {children:?}"));
        }
    }
    rows.push(format!("trivial derives {trivial:?} unported {unported}"));
}

/// `check` given each fixture: every UI test's source and [`SHAPES`], each the one file of a
/// crate with core, the crate's context made as a check makes it.
fn each_fixture(mut check: impl FnMut(&str, &Cx<'_, '_>)) {
    const CORE: &str = "copy, clone, fmt, derive, default, eq, ord, future, fn, from, \
                        unary_ops, builtin_impls, phantom_data, panic, coerce_unsized, \
                        dispatch_from_dyn";
    for &(name, source) in UI.iter().chain([("shapes", SHAPES), ("marks", MARKS)].iter()) {
        let fixture = format!("//- minicore: {CORE}\n//- /main.rs crate:main edition:2021\n{source}");
        let (db, names, _) = crate::tests::fixture_db(&fixture, "");
        blaze_query::db::attach(&db, || {
            let krate = hir::check::workspace_crates(&db)[0];
            let frame = crate::tests::frame();
            let cx = Cx::new(&db, krate, crate::Files::new(&names, ""), &frame);
            check(name, &cx);
        });
    }
}

/// The refinement: the crate's numbering made from def maps, item trees and each body's items
/// ([`Defs::of`]) is the syntax walk's ([`reference::Numbering`]) on every fixture of the
/// pass: each definition at the same number with the same id, kind, name and parent, each
/// module's items, the owners and every definition's children in the same order. What the
/// pass reports is read off this numbering in its order, so equal numberings report the same.
/// Control: a numbering that puts the impls of builtin derives after their module's items
/// ([`Order::DerivesLast`]) differs from it on [`SHAPES`].
#[test]
fn the_numbering_equals_the_syntax_walk() {
    let mut planted_caught = false;
    each_fixture(|name, cx| {
        let expected = reference_rows(&reference::Numbering::of(cx)).join("\n");
        assert_eq!(rows(cx, &Defs::of(cx)).join("\n"), expected, "fixture {name}");
        let planted = rows(cx, &Defs::numbered(cx, Order::DerivesLast)).join("\n");
        planted_caught |= planted != expected;
    });
    assert!(planted_caught, "the planted reorder of derives went unseen on every fixture");
}

/// What the whole pass decides of `cx`'s crate with each definition walked as `how` says, as
/// text: of each definition whether it is live and its parent visited (before what is
/// deferred is walked, and at the end), the derives ignored, the self-assignments linted and
/// what is reported.
fn whole_pass(cx: &Cx<'_, '_>, how: How) -> String {
    let defs = Defs::of(cx);
    let levels = Levels::new();
    let effective = Effective::of(cx);
    let reach = Reach::of(cx, &defs, &effective);
    let k = Crate::new(cx, &defs, &levels, &reach);
    let (liveness, found) = mark::live_as(&k, seed::seeds(&k), how);
    let mut rows = Vec::new();
    for def in defs.all() {
        let parent = defs[def].parent;
        let (pre, last) = (&liveness.pre_deferred, &liveness.last);
        rows.push(format!(
            "{def}: live {} {} parent visited {} {}",
            pre.contains(def),
            last.contains(def),
            pre.parent_live(parent),
            last.parent_live(parent)
        ));
    }
    let mut ignored: Vec<_> =
        liveness.last.ignored.iter().map(|(&def, names)| (def, names.to_vec())).collect();
    ignored.sort_unstable();
    rows.push(format!("ignored {ignored:?}"));
    rows.push(format!("found {found:?}"));
    rows.push(format!("reported {:?}", report::report(&k, &liveness)));
    rows.join("\n")
}

/// The refinement: the pass that replays each store's recorded marks ([`mark::live`]) decides
/// what the walk over syntax and the marking body walk ([`How::Reference`]) decide, on every
/// fixture of the pass and [`MARKS`]: every definition live and visited alike, before and after
/// what is deferred, the same derives ignored, the same self-assignments linted and the same
/// findings reported. Control: a replay that drops every self-assignment differs from it on
/// [`MARKS`].
#[test]
fn the_marks_equal_the_whole_pass() {
    let mut planted_caught = false;
    each_fixture(|name, cx| {
        let expected = whole_pass(cx, How::Reference);
        assert_eq!(whole_pass(cx, How::Records), expected, "fixture {name}");
        planted_caught |= whole_pass(cx, How::RecordsDroppingSelfAssigns) != expected;
    });
    assert!(planted_caught, "the planted drop of self-assignments went unseen on every fixture");
}

/// Whether any database records the stores its walks run for: [`executed`] of every other
/// test reads this and no lock.
static RECORDING: AtomicBool = AtomicBool::new(false);
/// The databases that record, and the stores their walks ran for since the last take.
static RECORDED: blaze_core::lock::Lock<Vec<base_db::Nonce>> =
    blaze_core::lock::Lock::new("test.diagnostics.dead.recorded", Vec::new());
static EXECUTED: blaze_core::lock::Lock<Vec<(base_db::Nonce, ExpressionStoreOwnerId)>> =
    blaze_core::lock::Lock::new("test.diagnostics.dead.executed", Vec::new());
/// Whether any database has the planted bug: [`planted_text_len`] reads this and no lock.
static PLANTING: AtomicBool = AtomicBool::new(false);
static PLANTED: blaze_core::lock::Lock<Vec<base_db::Nonce>> =
    blaze_core::lock::Lock::new("test.diagnostics.dead.planted", Vec::new());

/// The walk of the store of `owner` ran for `db` (`store_marks`, as it executes).
pub(super) fn executed(db: &Db, owner: ExpressionStoreOwnerId) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let nonce = db.nonce_and_revision().0;
    if RECORDED.lock().contains(&nonce) {
        EXECUTED.lock().push((nonce, owner));
    }
}

/// The stores the walks of `db` ran for since the last take, each once, in the order first run.
fn take_executed(db: &Db) -> Vec<ExpressionStoreOwnerId> {
    let nonce = db.nonce_and_revision().0;
    let mut own: Vec<ExpressionStoreOwnerId> = Vec::new();
    EXECUTED.lock().retain(|&(of, owner)| {
        let mine = of == nonce;
        if mine && !own.contains(&owner) {
            own.push(owner);
        }
        !mine
    });
    own
}

/// The bug planted in `db`'s walks, if it has it: each store's marks carry the length of its
/// root's text ([`StoreMark::Planted`]), which a body edit changes and no mark should.
pub(super) fn planted_text_len(db: &Db, owner: ExpressionStoreOwnerId) -> Option<u32> {
    if !PLANTING.load(Ordering::Relaxed) || !PLANTED.lock().contains(&db.nonce_and_revision().0) {
        return None;
    }
    let (store, source_map) = ExpressionStore::with_source_map(db, owner);
    let root = match owner {
        ExpressionStoreOwnerId::Body(body) => Body::of(db, body).root_expr(),
        ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
            store.expr_roots().next()?
        }
    };
    let source = source_map.expr_syntax(root).ok()?;
    Some(u32::from(source.value.syntax_node_ptr().text_range().len()))
}

/// The edit loop's fixture: a body edited last in the file (so no other store moves), the
/// pass finding a dead function and a self-assignment elsewhere.
const EDITED: &str = r#"//- /main.rs crate:main edition:2021
struct Point { x: i32, y: i32 }
fn unused() {}
fn helper(p: &mut Point) { p.x = p.x; }
fn main() {
    let mut p = Point { x: 0, y: 0 };
    helper(&mut p);
    let _ = edited();
}
fn edited() -> u32 { 1 }
"#;

/// The edit of [`EDITED`]: a literal of the last body made longer, which changes no name.
fn edit_holds(planted: bool) -> Result<(), String> {
    let (mut db, names, first) = fixture_db(EDITED, "");
    let nonce = db.nonce_and_revision().0;
    RECORDED.lock().push(nonce);
    RECORDING.store(true, Ordering::Relaxed);
    if planted {
        PLANTED.lock().push(nonce);
        PLANTING.store(true, Ordering::Relaxed);
    }
    let options = Options::default();
    let marks = |db: &Db, owners: &[ExpressionStoreOwnerId]| -> Vec<Vec<StoreMark>> {
        blaze_query::db::attach(db, || owners.iter().map(|&it| store_marks(db, it, ()).to_vec()).collect())
    };
    let before = check_all(&db, &names, &options);
    let owners = take_executed(&db);
    assert!(
        owners.iter().any(|it| matches!(it, ExpressionStoreOwnerId::Body(_))),
        "the pass walked no body of the fixture: {owners:?}"
    );
    let marks_before = marks(&db, &owners);
    let text = db.file_text(first).text(&db).as_str().to_owned();
    let new = text.replacen("{ 1 }", "{ 12 }", 1);
    assert_ne!(new, text, "the fixture has the literal the edit lengthens");
    db.set_file_text(first, &new);
    let after = check_all(&db, &names, &options);
    let rerun = take_executed(&db);
    let marks_after = marks(&db, &owners);
    take_executed(&db);
    if rerun.len() > 1 || rerun.iter().any(|it| !matches!(it, ExpressionStoreOwnerId::Body(_))) {
        return Err(format!("a body edit walked more than its body again: {rerun:?}"));
    }
    for ((owner, was), now) in owners.iter().zip(&marks_before).zip(&marks_after) {
        if was != now {
            return Err(format!("{owner:?}: the marks {was:?} became {now:?}"));
        }
    }
    if format!("{before:?}") != format!("{after:?}") {
        return Err(format!("the findings {before:?} became {after:?}"));
    }
    Ok(())
}

/// The invariant: a body edit that changes no name the body's walk records walks that one
/// body again, its marks come out equal, and every other store's marks are the database's
/// kept ones, so the fold reads the same records and the pass finds the same. Control: marks
/// that carry the length of their store's text (a fact of the syntax, not of what is named)
/// differ after the same edit, and the oracle sees it.
#[test]
fn a_body_edit_with_equal_marks_reruns_no_fold() {
    if let Err(violation) = edit_holds(false) {
        panic!("{violation}");
    }
    assert!(edit_holds(true).is_err(), "the planted text length in the marks went unseen");
}

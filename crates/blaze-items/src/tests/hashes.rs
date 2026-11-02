//! The four hashes and the keys: what an edit changes and what it keeps.

use hir_def::{AdtId, DefWithBodyId, expr_store::Body, hir::Expr};
use hir_expand::fixture;
use hir_ty::db::{InternedClosure, InternedClosureId};

use super::{TestDB, edit, item, single};
use crate::{
    ItemHashes, ItemId, Resolved,
    hash::{self, Graph, Members},
    hashes, item_key, key, layout_hash, resolve,
};

fn of(db: &TestDB, path: &str) -> ItemHashes {
    hashes(db, item(db, path)).unwrap_or_else(|err| panic!("{}", err.describe(db)))
}

fn adt(db: &TestDB, path: &str) -> AdtId {
    match item(db, path) {
        ItemId::Adt(it) => it,
        other => panic!("`{path}` is {other:?}"),
    }
}

#[test]
fn formatting_keeps_every_hash_and_key() {
    let (mut db, file) = single(
        r#"
struct S { x: u32 }
fn f(s: &S) -> u32 { s.x + 1 }
"#,
    );
    let (f, s) = (of(&db, "f"), of(&db, "S"));
    let key = item_key(&db, item(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
// a comment
struct S {
    x: u32,
}

fn f(s: &S) -> u32 {
    // another
    s.x  +  1
}
"#,
    );
    assert_eq!(of(&db, "f"), f);
    assert_eq!(of(&db, "S"), s);
    assert_eq!(item_key(&db, item(&db, "f")), key);
}

#[test]
fn rename_keeps_hashes_and_changes_the_key() {
    let (mut db, file) = single(
        r#"
struct S { x: u32 }
fn f(s: &S) -> u32 { s.x }
fn g(s: &S) -> u32 { f(s) }
"#,
    );
    let (f, s, g) = (of(&db, "f"), of(&db, "S"), of(&db, "g"));
    let f_key = item_key(&db, item(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
struct T { x: u32 }
fn h(s: &T) -> u32 { s.x }
fn g(s: &T) -> u32 { h(s) }
"#,
    );
    assert_eq!(of(&db, "h"), f);
    assert_eq!(of(&db, "T"), s);
    assert_eq!(of(&db, "g"), g);
    assert_ne!(item_key(&db, item(&db, "h")), f_key);
}

#[test]
fn move_keeps_hashes_and_changes_the_key() {
    let (mut db, file) = single(
        r#"
pub(crate) fn f(x: u32) -> u32 { x * 2 }
"#,
    );
    let f = of(&db, "f");
    let key = item_key(&db, item(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
mod m {
    pub(crate) fn f(x: u32) -> u32 { x * 2 }
}
"#,
    );
    assert_eq!(of(&db, "m::f"), f);
    assert_ne!(item_key(&db, item(&db, "m::f")), key);
}

#[test]
fn a_visibility_is_relative_to_the_module() {
    let (mut db, file) = single(
        r#"
fn f() {}
mod m { pub(super) fn g() {} }
"#,
    );
    let (f, g) = (of(&db, "f"), of(&db, "m::g"));
    assert_ne!(f.interface, g.interface, "private at the root is not `pub(super)` in `m`");
    edit(
        &mut db,
        file,
        r#"
mod m { fn f() {} }
mod n { mod m { pub(super) fn g() {} } }
"#,
    );
    assert_eq!(of(&db, "m::f").interface, f.interface, "private is private wherever it is");
    assert_eq!(of(&db, "n::m::g").interface, g.interface);
}

#[test]
fn a_body_edit_keeps_the_interface_and_callers_code() {
    let (mut db, file) = single(
        r#"
fn f() -> u32 { 1 }
fn g() -> u32 { f() }
"#,
    );
    let (f, g) = (of(&db, "f"), of(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 { 2 }
fn g() -> u32 { f() }
"#,
    );
    let (f2, g2) = (of(&db, "f"), of(&db, "g"));
    assert_eq!(f2.interface, f.interface);
    assert_ne!(f2.code, f.code);
    assert_ne!(f2.deep, f.deep);
    assert_eq!(g2.code, g.code, "a caller's code names its callee by interface");
    assert_ne!(g2.deep, g.deep, "a caller's deep hash has its callee's body");
}

#[test]
fn a_signature_edit_changes_callers_code() {
    let (mut db, file) = single(
        r#"
fn f(x: u32) -> u32 { x }
fn g() -> u32 { f(1) }
"#,
    );
    let (f, g) = (of(&db, "f"), of(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn f(x: u64) -> u32 { x as u32 }
fn g() -> u32 { f(1) }
"#,
    );
    assert_ne!(of(&db, "f").interface, f.interface);
    assert_ne!(of(&db, "g").code, g.code);
}

#[test]
fn a_body_edit_executes_no_interface_form_of_another_item() {
    let (mut db, file) = single(
        r#"
fn f() -> u32 { 1 }
fn g() -> u32 { f() }
"#,
    );
    let _ = (of(&db, "f"), of(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 { 2 }
fn g() -> u32 { f() }
"#,
    );
    let g = item(&db, "g");
    let executed = db.log_executed(|| {
        crate::interface_hash(&db, g).unwrap();
    });
    assert!(
        !executed.iter().any(|query| query.contains("interface_form")),
        "re-ran forms: {executed:?}"
    );
}

#[test]
fn whitespace_inside_a_literal_is_code() {
    // #1326: a formatter that dropped the bytes of a literal with the whitespace around it.
    let (mut db, file) = single(
        r#"
fn f() -> &'static str { "a b" }
"#,
    );
    let f = of(&db, "f");
    edit(
        &mut db,
        file,
        r#"
fn f() -> &'static str { "a  b" }
"#,
    );
    assert_ne!(of(&db, "f").code, f.code);
}

#[test]
fn layout_is_deep() {
    let (mut db, file) = single(
        r#"
struct B { x: u8 }
struct A { b: B, y: u16 }
impl B { fn get(&self) -> u8 { self.x } }
"#,
    );
    let a = layout_hash(&db, adt(&db, "A")).unwrap();
    edit(
        &mut db,
        file,
        r#"
struct B { x: u8 }
struct A { b: B, y: u16 }
impl B { fn get(&self) -> u8 { self.x + 1 } }
"#,
    );
    assert_eq!(layout_hash(&db, adt(&db, "A")).unwrap(), a, "an impl is not in a layout");
    edit(
        &mut db,
        file,
        r#"
struct B { x: u64 }
struct A { b: B, y: u16 }
impl B { fn get(&self) -> u8 { self.x as u8 } }
"#,
    );
    assert_ne!(layout_hash(&db, adt(&db, "A")).unwrap(), a, "a field's field is in a layout");
}

#[test]
fn a_cycle_hashes_by_content() {
    let (mut db, file) = single(
        r#"
struct A { b: *const B, x: u8 }
struct B { a: *const A }
"#,
    );
    let (a, b) = (of(&db, "A"), of(&db, "B"));
    assert_ne!(a.interface, b.interface);
    edit(
        &mut db,
        file,
        r#"
struct B { a: *const A }
struct A { b: *const B, x: u8 }
"#,
    );
    assert_eq!(of(&db, "A"), a, "order of declaration is not content");
    edit(
        &mut db,
        file,
        r#"
struct A { b: *const C, x: u8 }
struct C { a: *const A }
"#,
    );
    assert_eq!(of(&db, "A"), a, "a rename inside a cycle is not content");
    assert_eq!(of(&db, "C"), b);
    edit(
        &mut db,
        file,
        r#"
struct A { b: *const C, x: u16 }
struct C { a: *const A }
"#,
    );
    assert_ne!(of(&db, "C").interface, b.interface, "an edit of one member is the whole cycle's");
}

fn members_of(db: &TestDB, graph: Graph, id: ItemId) -> Vec<ItemId> {
    match hash::members(db, graph, id) {
        Members::Alone => vec![id],
        Members::Table(it) => it.to_vec(),
        Members::Searched(it) => it,
    }
}

#[test]
fn the_crate_table_finds_each_search_s_component() {
    let (db, _) = single(
        r#"
struct A { b: *const B, x: u8 }
struct B { a: *const A, c: C }
struct C { x: u32 }
struct L { next: *const L }
trait Tr { fn m(&self) -> Self where Self: Sized; fn n(&self) -> u8 { 0 } }
impl Tr for C { fn m(&self) -> C { C { x: self.x } } }
fn even(n: u32) -> bool { if n == 0 { true } else { odd(n - 1) } }
fn odd(n: u32) -> bool { if n == 0 { false } else { even(n - 1) } }
fn uses(a: &A) -> u8 { a.x }
fn f() -> impl Sized {
    struct Inner { s: *const Inner }
    Inner { s: 0 as *const Inner }
}
"#,
    );
    let krate = fixture::test_crate(&db);
    let mut cycles = 0;
    for graph in [Graph::Interface, Graph::Layout, Graph::Deep] {
        let table = hash::find_components(&db, graph, krate);
        for id in key::crate_item_ids(&db, krate) {
            if !graph.node(id) {
                continue;
            }
            let from_table = match table.get(id) {
                None => panic!("{graph:?}: {id:?} was not searched"),
                Some([]) => vec![id],
                Some(members) => members.to_vec(),
            };
            let mut alone = hash::searched(&db, graph, krate, None, id);
            alone.sort_unstable();
            assert_eq!(from_table, alone, "{graph:?}: {id:?}");
            assert_eq!(members_of(&db, graph, id), alone, "{graph:?}: {id:?}");
            if alone.len() > 1 {
                cycles += 1;
            }
        }
    }
    // A and B in the interface and layout graphs; even and odd in the deep one.
    assert!(cycles >= 6, "{cycles}");
}

#[test]
fn a_cycle_is_hashed_once_for_all_its_members() {
    let (db, _) = single(
        r#"
struct A { b: *const B }
struct B { c: *const C }
struct C { a: *const A }
"#,
    );
    let ids = ["A", "B", "C"].map(|name| item(&db, name));
    let executed = db.log_executed(|| {
        for id in ids {
            crate::interface_hash(&db, id).unwrap();
        }
    });
    let units = executed.iter().filter(|it| it.contains("interface_unit")).count();
    assert_eq!(units, 1, "{executed:#?}");
    let [a, b, c] = ids.map(|id| crate::interface_hash(&db, id).unwrap());
    assert!(a != b && b != c && a != c, "each member has its own position");
}

#[test]
fn a_body_edit_that_calls_nothing_new_keeps_the_deep_components() {
    let (mut db, file) = single(
        r#"
fn f() -> u32 { g() + 1 }
fn g() -> u32 { 2 }
"#,
    );
    let before = crate::deep_hash(&db, item(&db, "f")).unwrap();
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 { g() + 3 }
fn g() -> u32 { 2 }
"#,
    );
    let f = item(&db, "f");
    let executed = db.log_executed(|| assert_ne!(crate::deep_hash(&db, f).unwrap(), before));
    assert!(executed.iter().any(|it| it.contains("deep_refs")), "{executed:#?}");
    assert!(!executed.iter().any(|it| it.contains("deep_components")), "{executed:#?}");
}

#[test]
fn an_edit_that_closes_a_cycle_joins_its_members() {
    let (mut db, file) = single(
        r#"
fn f() -> u32 { g() }
fn g() -> u32 { 2 }
"#,
    );
    let f = item(&db, "f");
    assert_eq!(members_of(&db, Graph::Deep, f), vec![f]);
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 { g() }
fn g() -> u32 { if false { f() } else { 2 } }
"#,
    );
    let (f, g) = (item(&db, "f"), item(&db, "g"));
    let mut both = vec![f, g];
    both.sort_unstable();
    assert_eq!(members_of(&db, Graph::Deep, f), both);
    assert_eq!(members_of(&db, Graph::Deep, g), both);
    assert_ne!(crate::deep_hash(&db, f).unwrap(), crate::deep_hash(&db, g).unwrap());
}

#[test]
fn type_name_observes_the_name() {
    let (mut db, file) = single(
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct S;
fn f() -> &'static str { intrinsics::type_name::<S>() }
"#,
    );
    let f = of(&db, "f");
    edit(
        &mut db,
        file,
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct T;
fn f() -> &'static str { intrinsics::type_name::<T>() }
"#,
    );
    assert_ne!(of(&db, "f").code, f.code, "`type_name` spells the name");
}

#[test]
fn callers_of_two_intrinsics_of_one_signature_differ_in_code() {
    let (db, _) = single(
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub unsafe fn simd_lt<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic]
    pub unsafe fn simd_ge<T, U>(x: T, y: T) -> U;
}
fn lt(a: u64) -> u64 { unsafe { intrinsics::simd_lt(a, 0) } }
fn ge(a: u64) -> u64 { unsafe { intrinsics::simd_ge(a, 0) } }
"#,
    );
    let (lt, ge) = (of(&db, "intrinsics::simd_lt"), of(&db, "intrinsics::simd_ge"));
    assert_ne!(lt.interface, ge.interface, "an intrinsic is what its name says");
    assert_ne!(of(&db, "lt").code, of(&db, "ge").code);
}

fn closures(db: &TestDB, f: ItemId) -> Vec<InternedClosureId<'_>> {
    let ItemId::Function(func) = f else { panic!("{f:?} is no fn") };
    let owner = DefWithBodyId::from(func);
    Body::of(db, owner)
        .exprs()
        .filter_map(|(expr, it)| match it {
            Expr::Closure { closure_kind, .. } => Some(InternedClosureId::new(
                db,
                InternedClosure { owner: owner.into(), expr, kind: closure_kind },
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_capture_edit_is_its_closure_s_alone() {
    // #906: a sibling closure stayed stale after one's captures changed.
    let (mut db, file) = single(
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c1 = || a;
    let c2 = || b;
    c1() + c2()
}
"#,
    );
    let f = item(&db, "f");
    let before: Vec<_> =
        closures(&db, f).into_iter().map(|it| crate::closure_code(&db, it).unwrap()).collect();
    edit(
        &mut db,
        file,
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c1 = || b;
    let c2 = || b;
    c1() + c2()
}
"#,
    );
    let f = item(&db, "f");
    let after: Vec<_> =
        closures(&db, f).into_iter().map(|it| crate::closure_code(&db, it).unwrap()).collect();
    assert_eq!(before.len(), 2);
    assert_ne!(after[0], before[0]);
    assert_eq!(after[1], before[1]);
}

#[test]
fn every_key_resolves_back() {
    let (db, _) = single(
        r#"
mod m {
    pub enum E { A, B(u8) }
    pub trait Tr { fn m(&self) -> u8; }
}
struct S;
impl m::Tr for S { fn m(&self) -> u8 { 0 } }
const _: () = ();
fn f() -> impl Sized {
    struct Inner;
    let c = || 1u8;
    c();
    Inner
}
"#,
    );
    for id in super::items(&db) {
        assert_eq!(resolve(&db, item_key(&db, id)), Some(Resolved::Item(id)), "{id:?}");
    }
    let f = item(&db, "f");
    for closure in closures(&db, f) {
        let loc = closure.loc(&db);
        let found = resolve(&db, key::closure_key(&db, closure));
        assert!(
            matches!(found, Some(Resolved::Closure { expr, .. }) if expr == loc.expr),
            "{found:?}"
        );
    }
    assert_eq!(
        resolve(&db, key::opaque_key(&db, f, 0)),
        Some(Resolved::Opaque { owner: f, index: 0 })
    );
    let ItemId::Adt(AdtId::EnumId(e)) = item(&db, "m::E") else { panic!() };
    for (_, &(variant, _)) in &e.enum_variants(&db).variants {
        assert_eq!(resolve(&db, key::variant_key(&db, variant)), Some(Resolved::Variant(variant)));
    }
    let ItemId::Function(func) = f else { panic!() };
    let mut inner = 0;
    for (_, block) in Body::of(&db, func.into()).blocks(&db) {
        for (_, module) in block.modules(&db) {
            for def in module.scope(&db).declarations() {
                if let hir_def::ModuleDefId::AdtId(it) = def {
                    let id = ItemId::Adt(it);
                    assert_eq!(resolve(&db, item_key(&db, id)), Some(Resolved::Item(id)));
                    inner += 1;
                }
            }
        }
    }
    assert_eq!(inner, 1);
}

#[test]
fn an_edit_before_a_block_keeps_its_items_keys() {
    let (mut db, file) = single(
        r#"
fn f() {
    let x = 1;
    { struct Inner; }
}
"#,
    );
    let key_of_inner = |db: &TestDB| {
        let ItemId::Function(func) = item(db, "f") else { panic!() };
        let mut keys = Vec::new();
        for (_, block) in Body::of(db, func.into()).blocks(db) {
            for (_, module) in block.modules(db) {
                for def in module.scope(db).declarations() {
                    if let hir_def::ModuleDefId::AdtId(it) = def {
                        keys.push(item_key(db, ItemId::Adt(it)));
                    }
                }
            }
        }
        keys
    };
    let before = key_of_inner(&db);
    assert_eq!(before.len(), 1);
    edit(
        &mut db,
        file,
        r#"
fn f() {
    let x = 1;
    let y = x + 2;
    { struct Inner; }
}
"#,
    );
    assert_eq!(key_of_inner(&db), before);
}

#[test]
fn keys_of_two_crates_differ() {
    let db = fixture::db(
        r#"
//- /a.rs crate:a
pub struct S;
//- /b.rs crate:b
pub struct S;
"#,
    )
    .into_db();
    let krates: Vec<_> = base_db::all_crates(&db).iter().copied().collect();
    let keys: Vec<_> = krates.iter().map(|&krate| key::crate_key(&db, krate)).collect();
    for (at, a) in keys.iter().enumerate() {
        assert!(!keys[at + 1..].contains(a), "{krates:?}");
    }
}

/// One package built twice in a graph with the same cfg but other dependencies (a build
/// dependency reached from the host and the target side) is two crates: its paths name other
/// things.
#[test]
fn a_crate_with_other_dependencies_has_another_key() {
    let key_of_a = |fixture: &str| {
        let db = fixture::db(fixture).into_db();
        let a = base_db::all_crates(&db)
            .iter()
            .copied()
            .find(|&it| {
                it.extra_data(&db).display_name.as_ref().is_some_and(|it| it.canonical_name().as_str() == "a")
            })
            .expect("crate `a`");
        key::crate_key(&db, a)
    };
    let alone = key_of_a("//- /a.rs crate:a\npub struct S;\n//- /d.rs crate:d\npub struct D;\n");
    let with_d = key_of_a("//- /a.rs crate:a deps:d\npub struct S;\n//- /d.rs crate:d\npub struct D;\n");
    let with_other_d =
        key_of_a("//- /a.rs crate:a deps:d\npub struct S;\n//- /d.rs crate:d cfg:x\npub struct D;\n");
    assert_ne!(alone, with_d, "a dependency more");
    assert_ne!(with_d, with_other_d, "the dependency's own key");
}

/// A graph holding one package twice, alike in all a key hashes (two roots of one name,
/// version, origin, kind, edition, cfg and no dependencies), stops with an error naming both.
#[test]
fn two_crates_of_one_key_stop_the_build_naming_both() {
    let mut db = fixture::db("//- /a.rs crate:a\npub struct S;\n//- /b.rs crate:b\npub struct S;\n").into_db();
    let krates: Vec<_> = base_db::all_crates(&db).iter().copied().collect();
    let a = krates[0];
    let (data, workspace) = (a.data(&db).clone(), a.workspace_data(&db).clone());
    let roots: Vec<_> = krates.iter().map(|it| it.data(&db).root_file_id).collect();
    let mut graph = base_db::CrateGraphBuilder::default();
    for root in roots {
        graph.add_crate_root(
            root,
            data.edition,
            Some(base_db::CrateDisplayName::from_canonical_name("twice")),
            Some("1.0.0".to_owned()),
            cfg::CfgOptions::default(),
            None,
            base_db::Env::default(),
            base_db::CrateOrigin::Library { repo: None, name: intern::Symbol::intern("twice") },
            Vec::new(),
            base_db::TargetKind::Lib { is_proc_macro: false },
            data.proc_macro_cwd.clone(),
            workspace.clone(),
        );
    }
    graph.set_in_db(&mut db);
    let clash = key::check_crate_keys(&db).expect_err("two crates of one key");
    assert!(clash.same_key, "{clash}");
    let message = clash.to_string();
    assert_eq!(message.matches("twice@1.0.0").count(), 2, "{message}");
    assert!(message.contains("one key"), "{message}");
}

#[test]
fn a_batch_of_keys_is_each_key_alone() {
    let (db, _) = single(
        r#"
mod m { pub struct A; pub fn f() {} pub enum E { X, Y } }
trait Tr { fn a(); const C: u8; type T; }
impl Tr for m::A { fn a() {} const C: u8 = 0; type T = u8; }
macro_rules! mac { () => {} }
extern "C" { fn ext(); }
const _: () = ();
"#,
    );
    let items = super::items(&db);
    assert!(items.len() >= 12, "{items:?}");
    for id in items {
        assert_eq!(item_key(&db, id), key::item_key_alone(&db, id), "{id:?}");
    }
}

#[test]
fn a_path_hashed_in_parts_is_its_string_hashed() {
    let (db, _) = single(
        r#"
mod m { pub struct A; pub fn f() {} pub enum E { X } }
trait Tr { fn a(); const C: u8; type T; }
impl Tr for m::A { fn a() {} const C: u8 = 0; type T = u8; }
macro_rules! mac { () => {} }
extern "C" { fn ext(); }
const _: () = ();
static S: u8 = 0;
union U { x: u8 }
"#,
    );
    let items = super::items(&db);
    assert!(items.len() >= 14, "{items:?}");
    for id in items {
        let mut whole = blaze_ids::Hasher::new("test path");
        whole.part(key::path(&db, id).as_bytes());
        let mut parts = blaze_ids::Hasher::new("test path");
        key::path_part(&db, id, &mut |bytes| {
            parts.update(bytes);
        });
        assert_eq!(whole.finish(), parts.finish(), "{id:?}: {}", key::path(&db, id));
    }
}

fn crate_items(db: &TestDB, krate: base_db::Crate) -> Vec<ItemId> {
    let mut items = Vec::new();
    for (_, module) in hir_def::nameres::crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                hir_def::ModuleDefId::FunctionId(it) => items.push(ItemId::Function(it)),
                hir_def::ModuleDefId::AdtId(it) => items.push(ItemId::Adt(it)),
                _ => {}
            }
        }
    }
    items
}

#[test]
fn a_key_says_its_crate_and_whether_it_is_in_a_body() {
    let db = fixture::db(
        r#"
//- /a.rs crate:a
pub struct S;
pub fn f() -> u8 {
    struct Inner;
    let c = || 1u8;
    c()
}
//- /b.rs crate:b
pub struct S;
"#,
    )
    .into_db();
    let mut inner = 0;
    for &krate in base_db::all_crates(&db).iter() {
        let prefix = key::crate_key(&db, krate).0[..8].to_vec();
        for id in crate_items(&db, krate) {
            let key = item_key(&db, id).0.0;
            assert_eq!(key[..8], prefix[..], "{id:?}");
            assert_eq!(key[8] & 0x80, 0, "{id:?} is in no body");
            let ItemId::Function(func) = id else { continue };
            for (_, block) in Body::of(&db, func.into()).blocks(&db) {
                for (_, module) in block.modules(&db) {
                    for def in module.scope(&db).declarations() {
                        let hir_def::ModuleDefId::AdtId(it) = def else { continue };
                        let key = item_key(&db, ItemId::Adt(it)).0.0;
                        assert_eq!(key[..8], prefix[..]);
                        assert_eq!(key[8] & 0x80, 0x80, "`Inner` is in a body");
                        inner += 1;
                    }
                }
            }
            for closure in closures(&db, id) {
                let key = key::closure_key(&db, closure).0.0;
                assert_eq!(key[..8], prefix[..]);
                assert_eq!(key[8] & 0x80, 0x80, "a closure is in a body");
                inner += 1;
            }
        }
    }
    assert_eq!(inner, 2);
}

#[test]
fn an_item_of_each_crate_resolves_in_its_own() {
    let db = fixture::db(
        r#"
//- /a.rs crate:a
pub struct S;
//- /b.rs crate:b
pub struct S;
//- /c.rs crate:c deps:a,b
pub struct S;
"#,
    )
    .into_db();
    let mut seen = 0;
    for &krate in base_db::all_crates(&db).iter() {
        for id in crate_items(&db, krate) {
            let key = item_key(&db, id);
            assert_eq!(resolve(&db, key), Some(Resolved::Item(id)), "{id:?}");
            let mut flipped = key;
            flipped.0.0[8] ^= 0x80;
            assert_eq!(resolve(&db, flipped), None, "{id:?} is in no body");
            let mut elsewhere = key;
            elsewhere.0.0[0] ^= 1;
            assert_eq!(resolve(&db, elsewhere), None, "{id:?} is in no other crate");
            seen += 1;
        }
    }
    assert!(seen >= 3, "{seen}");
}

fn signature_of(db: &TestDB, path: &str) -> crate::SignatureHash {
    let id = item(db, path);
    crate::signature_hash(db, id).unwrap_or_else(|err| panic!("{}", err.describe(db)))
}

#[test]
fn a_leak_changes_the_interface_hash_and_keeps_the_signature_hash() {
    for (before, after, path) in [
        ("fn g() -> impl Copy { 1u8 }", "fn g() -> impl Copy { 1u16 }", "g"),
        ("const fn g() -> u8 { 1 }", "const fn g() -> u8 { 2 }", "g"),
        ("const C: u8 = 1;", "const C: u8 = 2;", "C"),
        ("static S: u8 = 1;", "static S: u8 = 2;", "S"),
        ("enum E { A = 1, B }", "enum E { A = 2, B }", "E"),
    ] {
        let (mut db, file) = single(before);
        let (interface, signature) = (of(&db, path).interface, signature_of(&db, path));
        edit(&mut db, file, after);
        assert_ne!(of(&db, path).interface, interface, "{before} -> {after}");
        assert_eq!(signature_of(&db, path), signature, "{before} -> {after}");
    }
}

/// E4 of docs/v2/plan/lanes/firewall/notes.md: a constant without generics is its value, so an
/// edit of the `const fn` it calls that keeps the value reaches none of its users.
#[test]
fn a_constant_is_its_value() {
    let common = "const ONE: u32 = one();\nfn uses() -> u32 { ONE }\n";
    let (mut db, file) = single(&format!("const fn one() -> u32 {{ 1 }}\n{common}"));
    let (konst, uses) = (of(&db, "ONE"), of(&db, "uses"));
    edit(&mut db, file, &format!("const fn one() -> u32 {{ let _kept = 0; 1 }}\n{common}"));
    assert_eq!(of(&db, "ONE").interface, konst.interface, "`one` changed, the value did not");
    assert_eq!(of(&db, "ONE").code, konst.code);
    assert_eq!(of(&db, "uses").code, uses.code);
    edit(&mut db, file, &format!("const fn one() -> u32 {{ 2 }}\n{common}"));
    assert_ne!(of(&db, "ONE").interface, konst.interface);
    assert_ne!(of(&db, "uses").code, uses.code);
}

#[test]
fn a_signature_change_changes_the_signature_hash() {
    for (before, after, path) in [
        (
            "trait A {} trait B { fn b(&self) {} } impl A for u8 {} impl B for u8 {}\n\
             fn g() -> impl A { 1u8 }",
            "trait A {} trait B { fn b(&self) {} } impl A for u8 {} impl B for u8 {}\n\
             fn g() -> impl B { 1u8 }",
            "g",
        ),
        ("const fn g() -> u8 { 1 }", "fn g() -> u8 { 1 }", "g"),
        ("const C: u8 = 1;", "const C: u16 = 1;", "C"),
        ("static S: u8 = 1;", "static mut S: u8 = 1;", "S"),
        ("enum E { A = 1, B }", "enum E { A, B }", "E"),
        ("struct T; fn g(_: T) {}", "struct T(u8); fn g(_: T) {}", "g"),
    ] {
        let (mut db, file) = single(before);
        let signature = signature_of(&db, path);
        edit(&mut db, file, after);
        assert_ne!(signature_of(&db, path), signature, "{before} -> {after}");
    }
}

#[test]
fn a_constructor_key_resolves_to_its_constructor() {
    use hir_ty::next_solver::Ctor;
    let (db, _) = single(
        r#"
struct T(u8);
struct U;
struct R { x: u8 }
enum E { A(u8), B, C { x: u8 } }
"#,
    );
    let structs = [("T", true), ("U", true), ("R", false)];
    for (path, has) in structs {
        let ItemId::Adt(AdtId::StructId(it)) = item(&db, path) else { panic!("`{path}`") };
        let found = resolve(&db, key::ctor_key(&db, Ctor::Struct(it)));
        assert_eq!(found, has.then_some(Resolved::Ctor(Ctor::Struct(it))), "{path}");
    }
    let AdtId::EnumId(e) = adt(&db, "E") else { panic!("`E` is an enum") };
    let variants = &e.enum_variants(&db).variants;
    for (at, (_, &(variant, _))) in variants.iter().enumerate() {
        let found = resolve(&db, key::ctor_key(&db, Ctor::Enum(variant)));
        assert_eq!(found, (at != 2).then_some(Resolved::Ctor(Ctor::Enum(variant))), "{at}");
    }
}

#[test]
fn an_anonymous_constant_key_resolves_to_its_root() {
    let (db, _) = single("struct A([u8; 1 + 2]);\nfn f() -> [u8; 4] { [0; 4] }");
    let ItemId::Adt(AdtId::StructId(it)) = item(&db, "A") else { panic!("`A` is a struct") };
    let store = hir_def::ExpressionStoreOwnerId::VariantFields(it.into());
    let roots: Vec<_> = hir_def::expr_store::ExpressionStore::of(&db, store).expr_roots().collect();
    assert!(!roots.is_empty(), "the array length is a root of the fields' store");
    for expr in roots {
        let found = resolve(&db, key::anon_const_key_of(&db, store, expr));
        assert_eq!(found, Some(Resolved::AnonConst { store, expr }));
    }
}

/// A body whose MIR fails to lower has no hash, and the error a build stops with says why
/// lowering failed, not only that it did.
#[test]
fn mir_that_fails_to_lower_is_named_in_the_error() {
    let (db, _) = single("fn f() -> u32 { g() }\n");
    let id = item(&db, "f");
    let err = hashes(&db, id).expect_err("`f` calls a name that does not resolve");
    assert_eq!(err.what, "MIR that failed to lower", "{}", err.describe(&db));
    let ItemId::Function(func) = id else { panic!("`f` is {id:?}") };
    let owner = hir_ty::InferBodyId::DefWithBodyId(DefWithBodyId::from(func));
    let lowering = hir_ty::db::HirDatabase::mir_body(&db, owner).expect_err("`f`'s MIR fails to lower");
    let said = err.describe(&db);
    assert!(said.ends_with(&format!("MIR that failed to lower ({lowering:?})")), "{said}");
}

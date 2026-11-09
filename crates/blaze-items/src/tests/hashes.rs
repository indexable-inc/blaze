//! The four hashes and the keys: what an edit changes and what it keeps.

use hir_def::{AdtId, DefWithBodyId, ItemContainerId, Lookup, expr_store::Body, hir::Expr};
use hir_expand::fixture;
use hir_ty::db::{InternedClosure, InternedClosureId};

use super::{TestDB, edit, item, single};
use crate::{
    ItemHashes, ItemId, Resolved,
    hash::{self, Graph, Members},
    hashes, item_key, key, layout_hash, resolve, signature_hash,
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

/// A body reading `<T as Tr>::K` holds the value `K` has for `T`, which the impl `T` selects
/// gives: the alias spells `T` by its interface, which two types alike but for their name
/// share, so two impls one macro writes for two such types (hir-def's `query_ids!` over its
/// ids, reading `<$ty as Relocate>::KIND`) were one code with two compiled bodies. Bodies
/// alike that read one value, of one type or of two types alike, still share their code.
#[test]
fn a_trait_constant_is_spelled_by_its_value() {
    let (db, _) = single(
        r#"
trait Kind { const KIND: u8; }
macro_rules! ids {
    ($($ty:ident = $kind:literal),*) => {$(
        pub struct $ty(u32);
        impl Kind for $ty { const KIND: u8 = $kind; }
    )*};
}
ids!(A = 13, B = 0, C = 13);
trait Get { fn get() -> u8; }
macro_rules! get {
    ($($ty:ty),*) => {$(
        impl Get for $ty { fn get() -> u8 { <$ty as Kind>::KIND } }
    )*};
}
get!(A, B);
fn a() -> u8 { <A as Kind>::KIND }
fn a_again() -> u8 { <A as Kind>::KIND }
fn b() -> u8 { <B as Kind>::KIND }
fn c() -> u8 { <C as Kind>::KIND }
"#,
    );
    let code = |id: ItemId| {
        hash::code_hash(&db, id)
            .unwrap_or_else(|err| panic!("{}", err.describe(&db)))
            .expect("a body")
    };
    let gets: Vec<_> = super::items(&db)
        .into_iter()
        .filter(|&id| match id {
            ItemId::Function(it) => {
                matches!(it.lookup(&db).container, ItemContainerId::ImplId(_))
                    && key::path(&db, id).ends_with("::get")
            }
            _ => false,
        })
        .collect();
    assert_eq!(gets.len(), 2, "the two `get`s `get!` writes: {gets:?}");
    assert_ne!(code(gets[0]), code(gets[1]), "`KIND` is 13 for `A` and 0 for `B`");
    let (a, b, c) = (of(&db, "a").code, of(&db, "b").code, of(&db, "c").code);
    assert_ne!(a, b, "`A` and `B` are alike but for their name and `KIND`");
    assert_eq!(of(&db, "a_again").code, a, "one body written twice is one code");
    assert_eq!(c, a, "one value of two types alike is one code");
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
                it.extra_data(&db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.canonical_name().as_str() == "a")
            })
            .expect("crate `a`");
        key::crate_key(&db, a)
    };
    let alone = key_of_a("//- /a.rs crate:a\npub struct S;\n//- /d.rs crate:d\npub struct D;\n");
    let with_d =
        key_of_a("//- /a.rs crate:a deps:d\npub struct S;\n//- /d.rs crate:d\npub struct D;\n");
    let with_other_d = key_of_a(
        "//- /a.rs crate:a deps:d\npub struct S;\n//- /d.rs crate:d cfg:x\npub struct D;\n",
    );
    assert_ne!(alone, with_d, "a dependency more");
    assert_ne!(with_d, with_other_d, "the dependency's own key");
}

/// A graph holding one package twice, alike in all a key hashes (two roots of one name,
/// version, origin, kind, edition, cfg and no dependencies), stops with an error naming both.
#[test]
fn two_crates_of_one_key_stop_the_build_naming_both() {
    let mut db =
        fixture::db("//- /a.rs crate:a\npub struct S;\n//- /b.rs crate:b\npub struct S;\n")
            .into_db();
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

/// One package's targets of one name (`tests/fetch.rs`, `examples/fetch.rs`, ...) alike in
/// all else are as many crates as cargo has units: under `--all-targets` an example is built
/// with `--test`, so the crate is a `Test` of an `Example` target, and its key must not be the
/// integration test's. Giving the example the test's manifest kind brings the clash back.
#[test]
fn an_example_and_a_test_with_one_name_are_two_crates() {
    use base_db::TargetKind::{Bench, Bin, Example, Lib, Test};
    use blaze_query::db::Durability;
    // (what the crate is built as, the manifest target's kind)
    let graph = |kinds: &[(base_db::TargetKind, base_db::TargetKind)]| {
        let fixture: String = (0..kinds.len())
            .map(|at| format!("//- /{at}.rs crate:c{at}\npub struct S;\n"))
            .collect();
        let mut db = fixture::db(&fixture).into_db();
        let made: Vec<_> = base_db::all_crates(&db).iter().copied().collect();
        let (data, workspace) = (made[0].data(&db).clone(), made[0].workspace_data(&db).clone());
        let mut krates = Vec::new();
        for (&(target_kind, manifest_kind), it) in kinds.iter().zip(&made) {
            let data = base_db::BuiltCrateData {
                root_file_id: it.data(&db).root_file_id,
                dependencies: Vec::new(),
                origin: base_db::CrateOrigin::Local {
                    repo: None,
                    name: Some(intern::Symbol::intern("blaze-registry")),
                },
                target_kind,
                manifest_kind,
                ..data.clone()
            };
            let extra = base_db::ExtraCrateData {
                version: Some("0.0.0".to_owned()),
                display_name: Some(base_db::CrateDisplayName::from_canonical_name("fetch")),
                potential_cfg_options: None,
                built_for: base_db::BuiltFor::default(),
                build_script_runs: Box::new([]),
            };
            let cfg = cfg::CfgOptions::default();
            let env = base_db::Env::default();
            krates.push(base_db::Crate::create(
                &mut db,
                data,
                extra,
                workspace.clone(),
                cfg,
                env,
                Durability::LOW,
            ));
        }
        base_db::set_all_crates_with_durability(&mut db, krates.iter().copied(), Durability::LOW);
        (db, krates)
    };
    let kinds = [
        (Test, Test),
        (Test, Example),
        (Example, Example),
        (Bin, Bin),
        (Test, Bin),
        (Bench, Bench),
        (Test, Bench),
        (Test, Lib { is_proc_macro: false }),
    ];
    let (db, krates) = graph(&kinds);
    key::check_crate_keys(&db).unwrap_or_else(|clash| panic!("{clash}"));
    let keys: Vec<_> = krates.iter().map(|&krate| key::crate_key(&db, krate)).collect();
    for (at, a) in keys.iter().enumerate() {
        assert!(!keys[at + 1..].contains(a), "{:?} shares its key", kinds[at]);
    }

    let (db, _) = graph(&[(Test, Test), (Test, Test)]);
    let clash =
        key::check_crate_keys(&db).expect_err("a test and a test of one name are one crate");
    assert!(clash.same_key, "{clash}");
    let (db, _) = graph(&[(Test, Example), (Test, Example)]);
    let message = key::check_crate_keys(&db).expect_err("two examples of one name").to_string();
    assert!(
        message.contains("test of the example target"),
        "the error names the target's kind: {message}"
    );
}

/// One package built for the host, for `--target` of the host's own triple, for
/// `--target wasm32-wasip1` and for that triple's wasm proc macros, alike in all else a key
/// hashes (std's crates are: the same cfg and dependencies, only their `OUT_DIR` and build
/// script runs differ), is four crates of four keys, as cargo keeps four units. Before the
/// key covered what a crate is built for, a `--target` build stopped on `proc_macro@0.0.0`.
#[test]
fn one_package_built_for_the_host_a_target_and_wasm_proc_macros_has_a_key_for_each() {
    use blaze_query::db::Durability;
    let host = "aarch64-apple-darwin";
    let wasm = "wasm32-wasip1";
    let built_for = |platform: Option<&str>, macro_wasm: bool| base_db::BuiltFor {
        platform: platform.map(intern::Symbol::intern),
        macro_wasm,
    };
    let graph = |kinds: &[base_db::BuiltFor]| {
        let fixture: String = (0..kinds.len())
            .map(|at| format!("//- /{at}.rs crate:c{at}\npub struct S;\n"))
            .collect();
        let mut db = fixture::db(&fixture).into_db();
        let made: Vec<_> = base_db::all_crates(&db).iter().copied().collect();
        let (data, workspace) = (made[0].data(&db).clone(), made[0].workspace_data(&db).clone());
        let mut krates = Vec::new();
        for (built_for, it) in kinds.iter().zip(&made) {
            let data = base_db::BuiltCrateData {
                root_file_id: it.data(&db).root_file_id,
                dependencies: Vec::new(),
                origin: base_db::CrateOrigin::Lang(base_db::LangCrateOrigin::ProcMacro),
                ..data.clone()
            };
            let extra = base_db::ExtraCrateData {
                version: Some("0.0.0".to_owned()),
                display_name: Some(base_db::CrateDisplayName::from_canonical_name("proc_macro")),
                potential_cfg_options: None,
                built_for: built_for.clone(),
                build_script_runs: Box::new([]),
            };
            let (cfg, env) = (cfg::CfgOptions::default(), base_db::Env::default());
            krates.push(base_db::Crate::create(
                &mut db,
                data,
                extra,
                workspace.clone(),
                cfg,
                env,
                Durability::LOW,
            ));
        }
        base_db::set_all_crates_with_durability(&mut db, krates.iter().copied(), Durability::LOW);
        (db, krates)
    };
    let kinds = [
        built_for(None, false),
        built_for(Some(host), false),
        built_for(Some(wasm), false),
        built_for(Some(wasm), true),
    ];
    let (db, krates) = graph(&kinds);
    key::check_crate_keys(&db).unwrap_or_else(|clash| panic!("{clash}"));
    let keys: Vec<_> = krates.iter().map(|&krate| key::crate_key(&db, krate)).collect();
    for (at, a) in keys.iter().enumerate() {
        assert!(!keys[at + 1..].contains(a), "{} shares its key", kinds[at]);
    }

    // Two of one platform are still one crate twice, and the error says what they are built for.
    let (db, _) = graph(&[built_for(Some(wasm), true), built_for(Some(wasm), true)]);
    let clash = key::check_crate_keys(&db).expect_err("two builds for one platform are one crate");
    assert!(clash.same_key, "{clash}");
    assert!(clash.to_string().contains("for wasm32-wasip1 (proc macros)"), "{clash}");
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

/// E4 of docs/v2/plan/design/item-hash.md: a constant without generics is its value, so an
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
    let lowering =
        hir_ty::db::HirDatabase::mir_body(&db, owner).expect_err("`f`'s MIR fails to lower");
    assert!(
        matches!(lowering, hir_ty::mir::MirLowerError::MissingFunctionDefinition(..)),
        "a call of a name that does not resolve has no function: {lowering:?}"
    );
    let said = err.describe(&db);
    assert!(
        said.contains("MIR that failed to lower (the call of `g` at "),
        "the callee is named as written: {said}"
    );
    assert!(said.contains(":1:17 has no function: "), "the call's place is given: {said}");
    assert!(said.ends_with("its name resolves to nothing)"), "inference's reason is given: {said}");
}

/// MIR that fails to lower in a closure written in a body is named by where the closure is
/// written, then why it failed (rule 19): the item's hash covers the closure's code, so the
/// item is what has no hash, but the error says which body of it.
#[test]
fn mir_that_fails_to_lower_in_a_closure_names_the_closure_and_its_place() {
    let (db, _) = single("fn f() -> u32 {\n    let c = || -> u32 { g() };\n    0\n}\n");
    let err =
        hashes(&db, item(&db, "f")).expect_err("`f`'s closure calls a name that does not resolve");
    assert_eq!(err.what, "MIR that failed to lower", "{}", err.describe(&db));
    let said = err.describe(&db);
    assert!(
        said.contains("MIR that failed to lower (the closure at "),
        "the closure is named: {said}"
    );
    assert!(
        said.contains(":2:13: the call of `g` at "),
        "the closure's place, then the call: {said}"
    );
}

/// selfbuild2's T1 at its cause: `rustc_type_ir::binder::BoundVarIndexKind` had an error type
/// only while rustc_type_ir's proc macro `rustc_index_macros` was not built, as its field's type
/// `DebruijnIndex` is written by `rustc_index::newtype_index!`; another crate's test link read
/// it then. Here `Idx` is written by a proc macro: with its expanders the enum hashes; while
/// they are not built yet no item of the crate has a hash, whatever it reads (pmwait's
/// `Missed::MacroPending`), and the error names the crate and the proc macros it waits on;
/// with a proc macro that failed to build, the error type names the field and the proc macro
/// and why it has none; and given them back it hashes again.
#[test]
fn an_error_type_left_by_an_unbuilt_proc_macro_names_the_field_and_the_macro() {
    use base_db::{ProcMacroLoadingError, all_crates};
    use hir_expand::proc_macro::ProcMacros;

    let fx = fixture::db(
        "//- proc_macros: mirror\n//- /lib.rs crate:lib\n\
         proc_macros::mirror! { ; (u32) Idx struct pub }\npub enum K { A(Idx) }\n",
    );
    let mut db = fx.into_db();
    let k = item(&db, "K");
    signature_hash(&db, k)
        .unwrap_or_else(|err| panic!("with its proc macro built: {}", err.describe(&db)));
    let pm = all_crates(&db)
        .iter()
        .copied()
        .find(|it| it.loads_only_macros(&db))
        .expect("the proc macros' crate");
    let built =
        ProcMacros::get_for_crate(&db, pm).and_then(|it| it.macros()).expect("its macros").to_vec();

    ProcMacros::give(&mut db, [(pm, Err(ProcMacroLoadingError::NotYetBuilt))]);
    let pending =
        signature_hash(&db, k).expect_err("no item of `lib` hashes while `mirror!` is not built");
    let (err, said) = (pending, pending.describe(&db));
    assert_eq!(err.what, "an item of a crate whose proc macros are not built yet", "{said}");
    assert!(said.contains("Enum `lib::K` has no hash"), "the item is named: {said}");
    assert!(
        said.contains("`lib` was read before the proc macros of `proc_macros` were built"),
        "{said}"
    );

    ProcMacros::give(&mut db, [(pm, Err(ProcMacroLoadingError::FailedToBuild))]);
    let err =
        signature_hash(&db, k).expect_err("`Idx` does not resolve while `mirror!` has no expander");
    let said = err.describe(&db);
    assert_eq!(err.what, "a type with an error", "{said}");
    assert!(said.contains("Enum `lib::K` has no hash"), "the item is named: {said}");
    assert!(said.contains("field `A.0`"), "the field is named: {said}");
    assert!(
        said.contains("`proc_macros` (proc-macro failed to build)"),
        "the proc macro and why: {said}"
    );

    ProcMacros::give(&mut db, [(pm, Ok(built))]);
    signature_hash(&db, k)
        .unwrap_or_else(|err| panic!("with its proc macro given back: {}", err.describe(&db)));
    // The refusal keeps the proc macro it found pending: said once it is built, it still names it.
    let said = pending.describe(&db);
    assert!(
        said.contains("`lib` was read before the proc macros of `proc_macros` were built"),
        "{said}"
    );
}

/// An iterator for the RPITIT fixtures below: minicore has no adapter that borrows.
const RPITIT_PRELUDE: &str =
    "//- minicore: iterator, option, sized, unsize, coerce_unsized, dispatch_from_dyn
struct It<'a>(&'a [u32]);
impl Iterator for It<'_> { type Item = u32; fn next(&mut self) -> Option<u32> { None } }
struct Once(u32);
impl Iterator for Once { type Item = u32; fn next(&mut self) -> Option<u32> { None } }
";

fn rpitit_db(body: &str) -> (TestDB, base_db::FileId) {
    single(&format!("{RPITIT_PRELUDE}{body}"))
}

/// Every item of the fixture hashes, and the trait method that returns `impl Trait`, the impl
/// method that gives its hidden type and the callers through the trait and through `dyn`.
#[track_caller]
fn all_hash(db: &TestDB, what: &str) {
    for id in super::items(db) {
        if let Err(err) = hashes(db, id) {
            panic!("{what}: {}", err.describe(db));
        }
    }
}

/// A trait method returning `impl Trait` (RPITIT) has a hash in every shape of `self` and
/// lifetime: with no default body it defines no hidden type, rustc's synthesized associated
/// type with no value, and is hashed by its bounds; each impl's method gives its own.
#[test]
fn a_trait_method_returning_impl_trait_has_a_hash() {
    for (what, body) in [
        (
            "elided `&self`",
            "pub(crate) trait G { fn roots(&self) -> impl Iterator<Item = u32>; }
struct F<'a>(&'a [u32]);
impl G for F<'_> { fn roots(&self) -> impl Iterator<Item = u32> { It(self.0) } }
fn walk(g: &impl G) -> Option<u32> { g.roots().next() }
pub fn run(x: &[u32]) -> Option<u32> { walk(&F(x)) }",
        ),
        (
            "elided `&self` and another parameter",
            "pub(crate) trait G { fn roots(&self, n: u32) -> impl Iterator<Item = u32>; }
struct F;
impl G for F { fn roots(&self, n: u32) -> impl Iterator<Item = u32> { Once(n) } }
fn walk(g: &impl G) -> Option<u32> { g.roots(1).next() }
pub fn run() -> Option<u32> { walk(&F) }",
        ),
        (
            "explicit lifetime",
            "pub(crate) trait G { fn roots<'s>(&'s self) -> impl Iterator<Item = u32> + 's; }
struct F<'a>(&'a [u32]);
impl G for F<'_> {
    fn roots<'s>(&'s self) -> impl Iterator<Item = u32> + 's { It(self.0) }
}
fn walk(g: &impl G) -> Option<u32> { g.roots().next() }
pub fn run(x: &[u32]) -> Option<u32> { walk(&F(x)) }",
        ),
        (
            // sb-printer: rustc_type_ir's `Interner::item_bounds` (`EarlyBinder<_, impl ..>`).
            "nested in `Option`, a tuple, a reference and a `Box`",
            "pub struct Bx<T>(T);
pub(crate) trait G {
    fn roots(self) -> Option<impl Iterator<Item = u32>>;
    fn pair(&self) -> (u32, impl Iterator<Item = u32>);
    fn refd(&self) -> &impl Iterator<Item = u32>;
    fn boxed(&self) -> Bx<impl Iterator<Item = u32>>;
}
struct F(Once);
impl G for F {
    fn roots(self) -> Option<impl Iterator<Item = u32>> { Some(self.0) }
    fn pair(&self) -> (u32, impl Iterator<Item = u32>) { (1, Once(1)) }
    fn refd(&self) -> &impl Iterator<Item = u32> { &self.0 }
    fn boxed(&self) -> Bx<impl Iterator<Item = u32>> { Bx(Once(2)) }
}
fn walk(g: impl G) -> Option<u32> {
    let (_, mut p) = g.pair();
    let _ = (p.next(), g.refd(), g.boxed());
    match g.roots() { Some(mut it) => it.next(), None => None }
}
pub fn run() -> Option<u32> { walk(F(Once(3))) }",
        ),
        (
            "by-value `self`",
            "pub(crate) trait G { fn roots(self) -> impl Iterator<Item = u32>; }
struct F(u32);
impl G for F { fn roots(self) -> impl Iterator<Item = u32> { Once(self.0) } }
fn walk(g: impl G) -> Option<u32> { g.roots().next() }
pub fn run(x: u32) -> Option<u32> { walk(F(x)) }",
        ),
        (
            "default body, one impl keeps it and one overrides it",
            "pub(crate) trait G { fn roots(&self) -> impl Iterator<Item = u32> { Once(7) } }
struct F;
struct H<'a>(&'a [u32]);
impl G for F {}
impl G for H<'_> { fn roots(&self) -> impl Iterator<Item = u32> { It(self.0) } }
fn walk(g: &impl G) -> Option<u32> { g.roots().next() }
pub fn run(x: &[u32]) -> Option<u32> { let _ = walk(&F); walk(&H(x)) }",
        ),
        (
            "dyn-incompatible method left out of a `dyn` trait",
            "pub(crate) trait G {
    fn roots(&self) -> impl Iterator<Item = u32> where Self: Sized;
    fn len(&self) -> u32;
}
struct F;
impl G for F {
    fn roots(&self) -> impl Iterator<Item = u32> where Self: Sized { Once(1) }
    fn len(&self) -> u32 { 1 }
}
fn by_dyn(g: &dyn G) -> u32 { g.len() }
fn walk(g: &impl G) -> Option<u32> { g.roots().next() }
pub fn run() -> u32 { by_dyn(&F) + walk(&F).unwrap_or(0) }",
        ),
    ] {
        let (db, _) = rpitit_db(body);
        all_hash(&db, what);
    }
}

/// Rule 21: the trait's declaration holds no impl's hidden type, so each impl's comes from
/// that impl's method. Changing what one impl's method returns behind its `impl Trait`
/// changes that method's interface, keeps the trait method's and the other impl's, and two
/// impls with different hidden types never hash alike.
#[test]
fn an_impls_hidden_type_for_a_trait_methods_impl_trait_reaches_its_callers() {
    let text = |hidden: &str| {
        format!(
            "pub(crate) trait G {{ fn roots(&self) -> impl Iterator<Item = u32>; }}
struct F<'a>(&'a [u32]);
struct K;
impl G for F<'_> {{ fn roots(&self) -> impl Iterator<Item = u32> {{ {hidden} }} }}
impl G for K {{ fn roots(&self) -> impl Iterator<Item = u32> {{ Once(2) }} }}
fn walk(g: &impl G) -> Option<u32> {{ g.roots().next() }}
pub fn run(x: &[u32]) -> Option<u32> {{ walk(&F(x)) }}"
        )
    };
    let (mut db, file) = rpitit_db(&text("It(self.0)"));
    // The impls' `roots`, in the order the impls are written: `F`'s, then `K`'s.
    let roots = |db: &TestDB, impl_of: &str| {
        let found: Vec<ItemId> = super::items(db)
            .into_iter()
            .filter(|&id| {
                matches!(id, ItemId::Function(it) if matches!(
                    hir_def::Lookup::lookup(&it, db).container,
                    hir_def::ItemContainerId::ImplId(_)
                )) && key::path(db, id).ends_with("::roots")
            })
            .collect();
        let at = match impl_of {
            "F" => 0,
            "K" => 1,
            _ => unreachable!(),
        };
        assert_eq!(found.len(), 2, "{found:?}");
        hashes(db, found[at]).unwrap_or_else(|err| panic!("{}", err.describe(db)))
    };
    let (trait_roots, f_roots, k_roots, run) =
        (of(&db, "G::roots"), roots(&db, "F"), roots(&db, "K"), of(&db, "run"));
    assert_ne!(f_roots.interface, k_roots.interface, "two impls' hidden types hash apart");
    edit(&mut db, file, &format!("{RPITIT_PRELUDE}{}", text("Once(3)")));
    assert_eq!(of(&db, "G::roots"), trait_roots, "the trait holds no impl's hidden type");
    assert_eq!(roots(&db, "K"), k_roots, "the other impl is unchanged");
    assert_ne!(roots(&db, "F").interface, f_roots.interface, "the impl's hidden type changed");
    // `run` reaches the impl's method only once `walk` is instantiated: that is the
    // instance's code identity (hir-ty-identity's
    // `an_impls_hidden_type_for_a_trait_methods_impl_trait_is_in_its_callers_identity`), not
    // an item hash, which keeps a generic body's trait call by the trait's method.
    assert_eq!(of(&db, "run").code, run.code);
}

/// Rule 19: the site an error type is at is named by what holds the error, never a parameter
/// whose only error is the region of an elided lifetime (every elided lifetime of a signature
/// is an error region, and is hashed).
#[test]
fn an_error_type_names_its_site_in_a_function() {
    for (body, path, site) in [
        ("fn f(x: u32, y: Missing) -> u32 { x }", "f", "(parameter 1)"),
        ("trait T { fn f(&self) -> Missing; }", "T::f", "(the return type)"),
        (
            "trait T { fn f(&self) -> impl Iterator<Item = Missing>; }",
            "T::f",
            "(the bounds of `impl Trait` 0 of the return type)",
        ),
        (
            "fn f<X>(x: X) -> X where X: Iterator<Item = Missing> { x }",
            "f",
            "(a bound or where clause)",
        ),
        (
            "fn f() -> impl Sized { let x: Option<Missing> = None; x }",
            "f",
            "(the hidden type the body gives `impl Trait` 0 of the return type)",
        ),
        ("fn f() -> u32 { let _x: Option<Missing> = None; 0 }", "f", "(a type in the body's MIR)"),
    ] {
        let (db, _) = rpitit_db(body);
        let said = match hashes(&db, item(&db, path)) {
            Ok(_) => panic!("`{body}` hashed"),
            Err(err) => err.describe(&db),
        };
        assert!(said.contains(site), "`{body}`: {said}");
        assert!(said.contains(&format!("`ra_test_fixture::{path}`")), "`{body}`: {said}");
    }
}

/// rustc's tests/ui/hygiene/auxiliary/variants.rs: a `macro` writes an enum with a variant it
/// names and one it is handed, both `Variant`. They are two variants and two keys, each of
/// which resolves back to its own.
#[test]
fn variants_of_one_text_a_macro_writes_have_their_own_keys() {
    let (db, _) = single(
        r#"
#![feature(decl_macro)]
macro x($type_name:ident, $variant_name:ident) {
    pub enum $type_name {
        Variant = 0,
        $variant_name = 1,
    }
}
x!(MyEnum, Variant);
"#,
    );
    let e = super::items(&db)
        .into_iter()
        .find_map(|it| match it {
            ItemId::Adt(AdtId::EnumId(it)) => Some(it),
            _ => None,
        })
        .expect("`MyEnum`");
    let variants: Vec<_> = e.enum_variants(&db).variants.values().map(|&(it, _)| it).collect();
    assert_eq!(variants.len(), 2);
    let keys: Vec<_> = variants.iter().map(|&it| key::variant_key(&db, it)).collect();
    assert_ne!(keys[0], keys[1]);
    for (&variant, &key) in variants.iter().zip(&keys) {
        assert_eq!(resolve(&db, key), Some(Resolved::Variant(variant)));
    }
}

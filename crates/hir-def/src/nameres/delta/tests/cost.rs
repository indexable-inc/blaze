//! What an edit costs the maintainer (design §10), read off the `Work` it reports for the crate
//! (`Db::report_work`): an impl added costs its own fact whatever the crate's size, a name added
//! wakes exactly its readers, a deletion's region is what read the deleted fact, and a module
//! added moves no impl before it. Each edit is made in a crate of 10 modules and in one of 1000,
//! and costs the same in both.
//!
//! `Work`'s fields as the `nameres` maintainer fills them: `ops`, the round-0 ops that differ;
//! `region`, the groups run; `rebuilt`, the modules rebuilt; `woken`, the groups a changed
//! binding woke; `slices`, the query inputs set; `keys`, the `(module, name)` bindings that
//! changed; `moved`, the rows of the crate's module table taken out or put back.

use std::fmt::Write;

use blaze_dbsp::{Weight, Work};

use super::{Loaded, check_db, module_at, name, work_of};
use crate::nameres::{
    crate_local_def_map,
    delta::LastWrite,
    facts::tests::{
        Exempt,
        generated::{Gen, Model, Module, UseTail, Vis},
    },
};

/// A generated crate of `count` modules: the root and `count - 1` file modules below it, each
/// with a struct, a function and an import of the one before it. The last module is a leaf.
fn generated_crate(count: usize) -> Model {
    let root = Module { parent: None, name: "", inline: false, macro_use: false, items: Vec::new() };
    let mut model = Model { edition: "2021", modules: vec![root], macros: Vec::new() };
    for at in 1..count {
        // alloc: test setup, a module name the model keeps for the test
        let name: &'static str = Box::leak(format!("m{at}").into_boxed_str());
        let mut items = vec![
            Gen::Struct { name: "S", vis: Vis::Pub },
            Gen::Fn { name: "f", vis: Vis::Pub },
        ];
        if at > 1 {
            items.push(Gen::Use {
                vis: Vis::Private,
                path: format!("crate::m{}", at - 1),
                tail: UseTail::Alias("S", "T"),
            });
        }
        model.modules.push(Module { parent: Some(0), name, inline: false, macro_use: false, items });
        model.modules[0].items.push(Gen::Mod(at));
    }
    model
}

/// `fixture`, whose first file is `/lib.rs`, grown to `count` modules by file modules declared
/// after its own, each `body`.
fn with_filler(fixture: &str, count: usize, body: &str) -> String {
    let modules = fixture.matches("//- /").count();
    let lib = fixture.find("//- /lib.rs").expect("the fixture starts with `/lib.rs`");
    let lib_end = lib + fixture[lib..].find('\n').expect("a header line") + 1;
    let next = fixture[lib_end..].find("//- /").map_or(fixture.len(), |it| lib_end + it);
    let (mut mods, mut files) = (String::new(), String::new());
    for at in modules..count {
        _ = writeln!(mods, "mod filler{at};");
        _ = writeln!(files, "//- /filler{at}.rs\n{body}");
    }
    format!("{}{mods}{}{files}", &fixture[..next], &fixture[next..])
}

/// The `Work` of adding `impl S {}` to the leaf of a generated crate of `count` modules.
fn adding_an_impl(count: usize) -> (Loaded, Work) {
    let mut loaded = Loaded::new(&generated_crate(count).render());
    let krate = loaded.krate();
    let leaf = loaded.file(&format!("/m{}.rs", count - 1));
    let text = loaded.text(leaf) + "impl S {}\n";
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(leaf, &text));
    (loaded, work)
}

#[test]
fn adding_an_impl_costs_its_fact() {
    let (small, at_10) = adding_an_impl(10);
    let (large, at_1000) = adding_an_impl(1000);
    assert_eq!(at_10, at_1000, "the same edit in a crate 100 times larger costs the same");
    // An impl binds no name: its op goes straight into the leaf's kept scope, which is set,
    // with no group run and no row moved.
    assert_eq!(at_10, Work { ops: 1, slices: 1, ..Work::default() }, "{at_10:?}");

    for (loaded, leaf) in [(&small, "m9"), (&large, "m999")] {
        let (db, krate) = (&loaded.db, loaded.krate());
        let map = crate_local_def_map(db, krate).def_map(db);
        let leaf = module_at(db, krate, &[leaf]);
        assert_eq!(map.module(db, leaf).scope(db).impls().count(), 1, "the impl is in the leaf");
    }
    let mut failures = String::new();
    check_db(&small.db, "adding an impl", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn an_impl_added_before_another_keeps_their_order() {
    let mut loaded = Loaded::new(&generated_crate(10).render().replace("pub fn f() {}\n", "pub fn f() {}\nimpl S {}\n"));
    let krate = loaded.krate();
    let leaf = loaded.file("/m9.rs");
    // `T` is the leaf's alias of `m8::S`: a self type of another name keeps the old impl's id.
    let text = loaded.text(leaf).replace("pub struct S;\n", "pub struct S;\nimpl T {}\n");
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(leaf, &text));
    assert_eq!(work, Work { ops: 1, slices: 1, ..Work::default() }, "{work:?}");
    let mut failures = String::new();
    check_db(&loaded.db, "an impl added before another", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn a_body_edit_costs_nothing() {
    let mut loaded = Loaded::new(&generated_crate(10).render());
    let krate = loaded.krate();
    let leaf = loaded.file("/m9.rs");
    let text = loaded.text(leaf).replace("fn f() {}", "fn f() { let _ = 1; }");
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(leaf, &text));
    assert_eq!(work, Work::default(), "a body is not name resolution's");
}

/// The `Work` of adding `mod extra;` after every other module of a generated crate of `count`
/// modules, each with an impl, and the weights of the impls the write hands over.
fn adding_a_module(count: usize) -> (Loaded, Work, Vec<Weight>) {
    let fixture = generated_crate(count).render().replace("pub struct S;\n", "pub struct S;\nimpl S {}\n")
        + "//- /extra.rs\npub struct X;\nimpl X {}\n";
    let mut loaded = Loaded::new(&fixture);
    let krate = loaded.krate();
    let lib = loaded.file("/lib.rs");
    let text = loaded.text(lib) + "mod extra;\n";
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(lib, &text));
    let impls = loaded.db.with_state::<LastWrite, _>(|_, last| {
        last.crates.iter().filter(|it| it.krate == krate).flat_map(|it| &it.impls).map(|&(_, _, weight)| weight).collect()
    });
    (loaded, work, impls)
}

#[test]
fn a_module_added_moves_no_impl_before_it() {
    let (small, at_10, impls_10) = adding_a_module(10);
    let (_, at_1000, impls_1000) = adding_a_module(1000);
    assert_eq!(at_10, at_1000, "the same edit in a crate 100 times larger costs the same");
    // The root and the new module run; the root's row and the new one are taken out and put back.
    assert_eq!((at_10.region, at_10.rebuilt, at_10.woken, at_10.moved), (2, 2, 0, 3), "{at_10:?}");
    assert_eq!(impls_10, [1], "only the new module's impl is handed over: {at_10:?}");
    assert_eq!(impls_1000, [1], "as in the small crate");
    let mut failures = String::new();
    check_db(&small.db, "a module added", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

const GLOBBED: &str = r#"
//- /lib.rs
mod a;
mod b;
mod c;
mod d;
mod e;
//- /a.rs
pub struct A;
//- /b.rs
use crate::a::*;
//- /c.rs
use crate::a::*;
//- /d.rs
use crate::a::*;
//- /e.rs
pub struct E;
use crate::a::A;
"#;

/// The `Work` of adding `foo` to `a` in [`GLOBBED`] grown to `count` modules, each of the others
/// reading `a::A` alone.
fn a_name_added(count: usize) -> (Loaded, Work) {
    let mut loaded = Loaded::new(&with_filler(GLOBBED, count, "use crate::a::A;"));
    let krate = loaded.krate();
    let a = loaded.file("/a.rs");
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(a, "pub struct A;\npub fn foo() {}\n"));
    (loaded, work)
}

#[test]
fn a_name_added_wakes_the_globs_that_read_it() {
    let (loaded, work) = a_name_added(10);
    let (_, at_1000) = a_name_added(1000);
    assert_eq!(work, at_1000, "the same edit in a crate 100 times larger costs the same");
    let krate = loaded.krate();
    // `b`, `c` and `d` glob `a`; `e` and the filler read `a::A` alone, which stays as it was.
    assert_eq!(work.woken, 3, "{work:?}");
    assert_eq!(work.region, 4, "`a` and its three globs: {work:?}");
    assert_eq!(work.keys, 4, "`foo` in `a`, `b`, `c` and `d`: {work:?}");
    // `a` out and back for the run that woke the globs, then the four out and back.
    assert_eq!(work.moved, 10, "{work:?}");
    for module in ["a", "b", "c", "d"] {
        let id = module_at(&loaded.db, krate, &[module]);
        let map = crate_local_def_map(&loaded.db, krate).def_map(&loaded.db);
        assert!(map.module(&loaded.db, id).name_in_scope(&loaded.db, &name("foo")).take_values().is_some(), "`{module}::foo`");
    }
    let mut failures = String::new();
    check_db(&loaded.db, "a name added", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

const CYCLE: &str = r#"
//- /lib.rs
mod p;
mod q;
mod r;
mod x;
mod y;
//- /p.rs
pub use crate::q::*;
pub struct Shared;
//- /q.rs
pub use crate::r::*;
//- /r.rs
pub use crate::p::*;
//- /x.rs
pub struct X;
//- /y.rs
use crate::x::X;
"#;

/// The `Work` of deleting `Shared` from `p` in [`CYCLE`] grown to `count` modules, each of the
/// others reading `x::X`.
fn deleting_shared(count: usize) -> (Loaded, Work) {
    let mut loaded = Loaded::new(&with_filler(CYCLE, count, "use crate::x::X;"));
    let krate = loaded.krate();
    for module in ["p", "q", "r"] {
        let id = module_at(&loaded.db, krate, &[module]);
        let map = crate_local_def_map(&loaded.db, krate).def_map(&loaded.db);
        assert!(map.module(&loaded.db, id).name_in_scope(&loaded.db, &name("Shared")).take_types().is_some(), "`{module}::Shared` before");
    }
    let p = loaded.file("/p.rs");
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(p, "pub use crate::q::*;\n"));
    (loaded, work)
}

#[test]
fn deleting_what_a_glob_cycle_shares_runs_the_cycle() {
    let (loaded, work) = deleting_shared(10);
    let (_, at_1000) = deleting_shared(1000);
    assert_eq!(work, at_1000, "the same edit in a crate 100 times larger costs the same");
    let krate = loaded.krate();
    // DRed's over-delete: the cycle reads itself, so it is in the region whole, and no glob of
    // it keeps `Shared` alive; `x`, `y` and the filler read nothing of it.
    assert_eq!(work.region, 3, "the cycle's groups: {work:?}");
    assert_eq!(work.keys, 3, "`Shared` in `p`, `q` and `r`: {work:?}");
    assert_eq!(work.moved, 6, "the cycle's rows out and back: {work:?}");
    for module in ["p", "q", "r"] {
        let id = module_at(&loaded.db, krate, &[module]);
        let map = crate_local_def_map(&loaded.db, krate).def_map(&loaded.db);
        assert!(map.module(&loaded.db, id).name_in_scope(&loaded.db, &name("Shared")).is_none(), "`{module}::Shared` after");
    }
    let mut failures = String::new();
    check_db(&loaded.db, "glob cycle deletion", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

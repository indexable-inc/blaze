//! Each form of fixture metadata, and the crate graph, source roots and files it gives.

use base_db::{
    Crate, CrateOrigin, Db, FileId, LangCrateOrigin, LibraryRoots, LocalRoots, SourceRootId,
    TargetKind, all_crates, target::Arch,
};
use cfg::CfgAtom;
use intern::Symbol;
use span::{Edition, TextRange, TextSize};
use test_utils::RangeOrOffset;

use super::{Fixture, db, db_with_proc_macros, test_crate};
use crate::proc_macro::{ProcMacro, ProcMacros};

fn named(db: &Db, name: &str) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|krate| {
            krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                == Some(name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn text(db: &Db, file: FileId) -> String {
    db.file_text(file).text(db).as_str().to_owned()
}

fn path(db: &Db, file: FileId) -> String {
    let root = db.file_source_root(file).source_root_id(db);
    db.source_root(root).source_root_ref(db).path_for_file(&file).expect("a path").to_string()
}

fn root_of(db: &Db, file: FileId) -> SourceRootId {
    db.file_source_root(file).source_root_id(db)
}

fn is_library(db: &Db, file: FileId) -> bool {
    db.source_root(root_of(db, file)).source_root_ref(db).is_library
}

/// The names of `krate`'s dependencies, each with whether it is in the extern prelude and
/// whether it is a sysroot one.
fn deps(db: &Db, krate: Crate) -> Vec<(String, bool, bool)> {
    krate
        .data(db)
        .dependencies
        .iter()
        .map(|dep| (dep.name.symbol().as_str().to_owned(), dep.is_prelude(), dep.is_sysroot()))
        .collect()
}

fn file_id(fixture: &Fixture, at: usize) -> FileId {
    fixture.file(at).file_id(fixture.db())
}

#[test]
fn a_fixture_without_metadata_is_main_rs_of_the_default_crate() {
    let fixture = db("fn main() {}\n");
    let db = fixture.db();
    let file = fixture.only_file();
    assert_eq!(fixture.paths(), ["/main.rs"]);
    assert_eq!(file.file_id(db), FileId::from_raw(0));
    assert_eq!(file.edition(db), Edition::CURRENT);
    assert_eq!(text(db, file.file_id(db)).trim_end(), "fn main() {}");
    assert_eq!(path(db, file.file_id(db)), "/main.rs");
    assert!(!is_library(db, file.file_id(db)));

    assert_eq!(all_crates(db).len(), 1);
    let krate = fixture.krate();
    assert_eq!(krate, named(db, "ra_test_fixture"));
    assert_eq!(krate, test_crate(db));
    let data = krate.data(db);
    assert_eq!(data.root_file_id, file.file_id(db));
    assert_eq!(data.edition, Edition::CURRENT);
    assert_eq!(data.origin, CrateOrigin::Local { repo: None, name: None });
    assert_eq!(data.target_kind, TargetKind::Lib { is_proc_macro: false });
    assert!(data.dependencies.is_empty());
    assert_eq!(krate.env(db).get("__ra_is_test_fixture").as_deref(), Some("__ra_is_test_fixture"));
    assert_eq!(data.proc_macro_cwd.as_str(), "/");
    assert!(fixture.marked().is_none());
    assert!(fixture.sysroot_files().is_empty());
}

#[test]
fn files_have_ids_in_the_fixture_order_and_their_lines() {
    let fixture = db(r#"
//- /lib.rs
mod a;
mod b;
//- /a.rs
pub struct A;
//- /b/mod.rs
pub struct B;
"#);
    let db = fixture.db();
    assert_eq!(fixture.paths(), ["/lib.rs", "/a.rs", "/b/mod.rs"]);
    for at in 0..3 {
        assert_eq!(file_id(&fixture, at), FileId::from_raw(at as u32));
        assert_eq!(path(db, file_id(&fixture, at)), fixture.paths()[at]);
    }
    assert_eq!(fixture.file_at("/a.rs"), fixture.file(1));
    assert_eq!(text(db, file_id(&fixture, 1)), "pub struct A;\n");
    assert_eq!(fixture.lines().len(), 3);
    assert!(fixture.lines().windows(2).all(|it| it[0] < it[1]), "{:?}", fixture.lines());
    // One source root, local, with all three.
    let root = root_of(db, file_id(&fixture, 0));
    assert!((0..3).all(|at| root_of(db, file_id(&fixture, at)) == root));
    assert_eq!(root, SourceRootId(0));
    assert_eq!(LocalRoots::get(db).roots(db).iter().copied().collect::<Vec<_>>(), [SourceRootId(0)]);
    assert!(LibraryRoots::get(db).roots(db).is_empty());
    // The default crate's root is `/lib.rs`.
    assert_eq!(fixture.krate().data(db).root_file_id, file_id(&fixture, 0));
}

#[test]
fn crates_deps_and_extern_prelude() {
    let fixture = db(r#"
//- /main.rs crate:main deps:foo,bar-baz extern-prelude:foo
fn main() {}
//- /foo.rs crate:foo@0.1.0,https://example.com/foo.git
pub fn foo() {}
//- /bar.rs crate:bar-baz
pub fn bar() {}
"#);
    let db = fixture.db();
    assert_eq!(all_crates(db).len(), 3);
    let main = named(db, "main");
    let foo = named(db, "foo");
    let bar = named(db, "bar_baz");
    assert_eq!(fixture.krate(), main);
    assert_eq!(main.data(db).root_file_id, file_id(&fixture, 0));
    assert_eq!(
        deps(db, main),
        [("foo".to_owned(), true, false), ("bar_baz".to_owned(), false, false)]
    );
    assert_eq!(main.data(db).dependencies[0].crate_id, foo);
    assert_eq!(main.data(db).dependencies[1].crate_id, bar);
    assert_eq!(foo.extra_data(db).version.as_deref(), Some("0.1.0"));
    assert_eq!(
        foo.data(db).origin,
        CrateOrigin::Local {
            repo: Some("https://example.com/foo.git".to_owned()),
            name: Some(Symbol::intern("foo")),
        }
    );
    assert!(foo.data(db).dependencies.is_empty());
    // A crate of its own has the fixture's env variable only through its own `env:`.
    assert_eq!(main.env(db).get("__ra_is_test_fixture"), None);
}

#[test]
fn edition_cfg_env_and_crate_attrs() {
    let fixture = db(r#"
//- /main.rs crate:main edition:2018 cfg:test,feature=foo env:KEY=value crate-attr:no_std crate-attr:feature(never_type)
fn main() {}
"#);
    let db = fixture.db();
    let main = fixture.krate();
    assert_eq!(fixture.file(0).edition(db), Edition::Edition2018);
    assert_eq!(main.data(db).edition, Edition::Edition2018);
    let cfg = main.cfg_options(db);
    assert!(cfg.check_atom(&CfgAtom::Flag(Symbol::intern("test"))));
    assert!(cfg.check_atom(&CfgAtom::KeyValue {
        key: Symbol::intern("feature"),
        value: Symbol::intern("foo"),
    }));
    assert!(!cfg.check_atom(&CfgAtom::Flag(Symbol::intern("bench"))));
    assert_eq!(main.extra_data(db).potential_cfg_options.as_ref(), Some(cfg));
    assert_eq!(main.env(db).get("KEY").as_deref(), Some("value"));
    let attrs: Vec<&str> = main.data(db).crate_attrs.iter().map(|it| &**it).collect();
    assert_eq!(attrs, ["#![no_std]", "#![feature(never_type)]"]);
}

#[test]
fn the_default_crate_takes_its_files_edition_cfg_and_env() {
    let fixture = db(r#"
//- /lib.rs edition:2015 cfg:foo env:A=b
"#);
    let db = fixture.db();
    let krate = fixture.krate();
    assert_eq!(krate.data(db).edition, Edition::Edition2015);
    assert!(krate.cfg_options(db).check_atom(&CfgAtom::Flag(Symbol::intern("foo"))));
    assert_eq!(krate.env(db).get("A").as_deref(), Some("b"));
    assert_eq!(krate.env(db).get("__ra_is_test_fixture").as_deref(), Some("__ra_is_test_fixture"));
}

#[test]
fn target_kinds_and_lang_origins() {
    let fixture = db(r#"
//- /main.rs crate:bin:app deps:helper,core
//- /helper.rs crate:proc-macro:helper
//- /lib.rs crate:lib:plain
//- /test.rs crate:test:t
//- /core.rs crate:core
//- /other_core.rs crate:r#alloc
"#);
    let db = fixture.db();
    assert_eq!(named(db, "app").data(db).target_kind, TargetKind::Bin);
    assert_eq!(named(db, "helper").data(db).target_kind, TargetKind::Lib { is_proc_macro: true });
    assert_eq!(named(db, "plain").data(db).target_kind, TargetKind::Lib { is_proc_macro: false });
    assert_eq!(named(db, "t").data(db).target_kind, TargetKind::Test);
    let core = named(db, "core");
    assert_eq!(core.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Core));
    assert_eq!(
        named(db, "alloc").data(db).origin,
        CrateOrigin::Local { repo: None, name: Some(Symbol::intern("alloc")) }
    );
    // A dependency on a language crate is a sysroot one.
    assert_eq!(
        deps(db, named(db, "app")),
        [("helper".to_owned(), true, false), ("core".to_owned(), true, true)]
    );
    assert_ne!(fixture.krate(), core);
}

#[test]
fn new_source_roots_and_library_crates() {
    let fixture = db(r#"
//- /main.rs crate:main deps:dep,member
fn main() {}
//- /dep.rs crate:dep new_source_root:library
pub fn dep() {}
//- /dep2.rs
//- /member.rs crate:member new_source_root:local library
"#);
    let db = fixture.db();
    let [main, dep, dep2, member] = [0, 1, 2, 3].map(|at| file_id(&fixture, at));
    assert_eq!([main, dep, dep2, member].map(|it| it.index()), [0, 1, 2, 3]);
    assert_eq!(root_of(db, main), SourceRootId(0));
    assert_eq!(root_of(db, dep), SourceRootId(1));
    assert_eq!(root_of(db, dep2), SourceRootId(1));
    assert_eq!(root_of(db, member), SourceRootId(2));
    assert!(!is_library(db, main));
    assert!(is_library(db, dep));
    assert!(!is_library(db, member));
    assert!(LibraryRoots::get(db).roots(db).contains(&SourceRootId(1)));
    assert!(LocalRoots::get(db).roots(db).contains(&SourceRootId(0)));
    assert!(LocalRoots::get(db).roots(db).contains(&SourceRootId(2)));
    // A crate in a library root, or marked `library`, is no workspace member.
    assert_eq!(
        named(db, "dep").data(db).origin,
        CrateOrigin::Library { repo: None, name: Symbol::intern("dep") }
    );
    assert_eq!(
        named(db, "member").data(db).origin,
        CrateOrigin::Library { repo: None, name: Symbol::intern("member") }
    );
    assert_eq!(text(db, dep), "pub fn dep() {}\n");
}

#[test]
fn toolchain_target_data_layout_and_arch() {
    let fixture = db(r#"
//- toolchain: nightly
//- target_data_layout: e-m:e-p:32:32-i64:64-n32:64-S128
//- target_arch: wasm32
//- /main.rs
"#);
    let db = fixture.db();
    let workspace = fixture.krate().workspace_data(db);
    assert_eq!(workspace.toolchain.as_ref().map(|it| it.to_string()).as_deref(), Some("1.76.0-nightly"));
    let target = workspace.target.as_ref().expect("the fixture's target");
    assert_eq!(&*target.data_layout, "e-m:e-p:32:32-i64:64-n32:64-S128");
    assert_eq!(target.arch, Arch::Wasm32);
    assert_eq!(workspace.const_eval.steps, 10_000_000);

    let stable = super::db("//- /main.rs\n");
    let workspace = stable.krate().workspace_data(stable.db());
    assert_eq!(workspace.toolchain.as_ref().map(|it| it.to_string()).as_deref(), Some("1.76.0-stable"));
    assert_eq!(workspace.target.as_ref().expect("a target").arch, Arch::Other);
}

#[test]
fn minicore_is_core_after_the_fixtures_files() {
    let fixture = db(r#"
//- minicore: option
//- /main.rs crate:main
fn f() -> Option<i32> { None }
//- /dep.rs crate:dep new_source_root:library
"#);
    let db = fixture.db();
    let [core_file] = fixture.sysroot_files() else { panic!("{:?}", fixture.sysroot_files()) };
    assert_eq!(core_file.index(), 2);
    assert_eq!(path(db, *core_file), "/sysroot/core/lib.rs");
    assert!(is_library(db, *core_file));
    assert!(text(db, *core_file).contains("enum Option"));
    // Root IDs: the fixture's first root, minicore's, then the fixture's last.
    assert_eq!(root_of(db, file_id(&fixture, 0)), SourceRootId(0));
    assert_eq!(root_of(db, *core_file), SourceRootId(1));
    assert_eq!(root_of(db, file_id(&fixture, 1)), SourceRootId(2));
    let core = named(db, "core");
    assert_eq!(core.data(db).root_file_id, *core_file);
    assert_eq!(core.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Core));
    for name in ["main", "dep"] {
        assert_eq!(deps(db, named(db, name)), [("core".to_owned(), true, true)], "{name}");
    }
    assert!(core.data(db).dependencies.is_empty());
    assert_eq!(fixture.krate(), named(db, "main"));

    let default = super::db("//- minicore: option\n//- /lib.rs\n");
    let krate = default.krate();
    assert_eq!(deps(default.db(), krate), [("core".to_owned(), true, true)]);
}

#[test]
fn proc_macros_are_a_crate_every_crate_depends_on() {
    let fixture = db(r#"
//- proc_macros: identity, mirror, derive_identity
//- minicore: derive
//- /main.rs crate:main deps:dep
//- /dep.rs crate:dep
"#);
    let db = fixture.db();
    let [core_file, macros_file] = fixture.sysroot_files() else { panic!() };
    assert_eq!([core_file.index(), macros_file.index()], [2, 3]);
    assert_eq!(path(db, *macros_file), "/sysroot/proc_macros/lib.rs");
    assert_eq!(root_of(db, *core_file), SourceRootId(0));
    assert_eq!(root_of(db, *macros_file), SourceRootId(1));
    assert_eq!(root_of(db, file_id(&fixture, 0)), SourceRootId(2));
    let source = text(db, *macros_file);
    assert!(source.contains("pub fn identity"));
    assert!(source.contains("pub fn mirror"));
    assert!(source.contains("pub fn derive_identity"));
    assert!(!source.contains("pub fn shorten"));

    let macros = named(db, "proc_macros");
    assert_eq!(macros.data(db).target_kind, TargetKind::Lib { is_proc_macro: true });
    for name in ["main", "dep", "core"] {
        assert!(
            deps(db, named(db, name)).contains(&("proc_macros".to_owned(), true, false)),
            "{name}: {:?}",
            deps(db, named(db, name))
        );
    }
    let loaded = ProcMacros::get_for_crate(db, macros).expect("the proc macros' crate has macros");
    let mut names: Vec<&str> =
        loaded.macros().expect("they loaded").iter().map(|it| it.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["DeriveIdentity", "identity", "mirror"]);
}

#[test]
fn every_test_proc_macro_is_there() {
    let fixture = db(r#"
//- proc_macros: identity, derive_identity, input_replace, mirror, shorten, issue_18089, issue_18840, issue_17479, issue_18898, disallow_cfg, generate_suffixed_type
//- /main.rs crate:main
"#);
    let db = fixture.db();
    let loaded = ProcMacros::get_for_crate(db, named(db, "proc_macros")).expect("macros");
    assert_eq!(loaded.macros().expect("they loaded").len(), 11);
}

#[test]
fn extra_proc_macros_are_named_like_the_test_ones() {
    let identity = super::default_test_proc_macros().into_iter().next().expect("identity").1;
    let extra = ProcMacro { name: Symbol::intern("MyExtra"), ..identity };
    let fixture = db_with_proc_macros(
        "//- proc_macros: my_extra\n//- /main.rs crate:main\n",
        vec![("pub fn my_extra() {}\n".to_owned(), extra)],
    );
    let db = fixture.db();
    let loaded = ProcMacros::get_for_crate(db, named(db, "proc_macros")).expect("macros");
    let names: Vec<&str> = loaded.macros().expect("loaded").iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["MyExtra"]);
    assert_eq!(text(db, fixture.sysroot_files()[0]), "pub fn my_extra() {}\n");
}

#[test]
fn a_fixture_proc_macro_crate_has_the_macros_it_is_given() {
    let mut fixture = db(r#"
//- /main.rs crate:main deps:macros
//- /macros.rs crate:proc-macro:macros
"#);
    let macros = named(fixture.db(), "macros");
    assert!(ProcMacros::get_for_crate(fixture.db(), macros).is_none());
    let identity = super::default_test_proc_macros().into_iter().next().expect("identity").1;
    fixture.set_proc_macros(macros, vec![ProcMacro { name: Symbol::intern("Given"), ..identity }]);
    let db = fixture.db();
    let loaded = ProcMacros::get_for_crate(db, macros).expect("the crate was given macros");
    let names: Vec<&str> = loaded.macros().expect("loaded").iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["Given"]);
    assert!(ProcMacros::get_for_crate(db, named(db, "main")).is_none());
}

#[test]
fn a_position_a_range_and_an_escaped_marker() {
    let fixture = db(r#"
//- /main.rs
fn main() {}
//- /lib.rs crate:other
fn f$0() {}
"#);
    let position = fixture.position();
    assert_eq!(position.file_id, fixture.file(1));
    assert_eq!(position.offset, TextSize::from(4));
    assert_eq!(text(fixture.db(), file_id(&fixture, 1)), "fn f() {}\n");

    let fixture = db("fn $0main$0() {}");
    let range = fixture.range();
    assert_eq!(range.file_id, fixture.only_file());
    assert_eq!(range.range, TextRange::new(TextSize::from(3), TextSize::from(7)));
    let (_, marked) = fixture.range_or_offset();
    assert!(matches!(marked, RangeOrOffset::Range(_)));

    let fixture = db(r#"fn main() { let _ = "\$0"; }"#);
    assert!(fixture.marked().is_none());
    assert_eq!(text(fixture.db(), file_id(&fixture, 0)).trim_end(), r#"fn main() { let _ = "$0"; }"#);
}

#[test]
fn an_edit_after_setup_is_set_file_text() {
    let mut fixture = db("//- /main.rs\nfn a() {}\n//- /b.rs\nfn b() {}\n");
    let file = file_id(&fixture, 1);
    fixture.db_mut().set_file_text(file, "fn c() {}\n");
    let db = fixture.into_db();
    assert_eq!(text(&db, file), "fn c() {}\n");
    assert_eq!(path(&db, file), "/b.rs");
}

#[test]
#[should_panic(expected = "twice")]
fn a_path_twice_is_refused() {
    db("//- /main.rs crate:a\n//- /main.rs crate:b\n");
}

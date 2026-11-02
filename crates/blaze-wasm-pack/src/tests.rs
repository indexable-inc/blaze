use base_db::Db;
use blaze_query::db::Durability;
use hir_expand::{change::ChangeWithProcMacros, proc_macro::ProcMacros};

use super::*;

fn krate(root: u32, name: &str, origin: Origin, deps: Vec<Dep>, macros: Option<Vec<Macro>>) -> Krate {
    Krate {
        root,
        edition: "2021".to_owned(),
        name: Some(name.to_owned()),
        version: Some("1.0.0".to_owned()),
        cfg: vec!["panic=abort".to_owned(), "target_arch=wasm32".to_owned()],
        env: vec![("CARGO_PKG_NAME".to_owned(), name.to_owned())],
        origin,
        attrs: Vec::new(),
        deps,
        macros,
    }
}

fn dep(krate: usize, name: &str) -> Dep {
    Dep { krate, name: name.to_owned(), prelude: true, sysroot: false }
}

/// `app` on `lib` and the proc macro crate `derive`, with one recorded expansion.
fn pack() -> Pack {
    let file = |id: u32, path: &str, text: &str| File { id, path: path.to_owned(), text: text.to_owned() };
    Pack {
        main: 2,
        tuple: "wasm32-unknown-unknown".to_owned(),
        data_layout: "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20".to_owned(),
        files: vec![
            file(0, "/lib/src/lib.rs", "pub fn one() -> u32 { 1 }\n"),
            file(1, "/proc-macro/1.rs", "pub fn Show() {}\n"),
            file(2, "/app/src/main.rs", "#[derive(derive::Show)] struct S;\npub fn two() -> u32 { lib::one() + 1 }\n"),
        ],
        roots: vec![
            Root { library: true, files: vec![0] },
            Root { library: true, files: vec![1] },
            Root { library: false, files: vec![2] },
        ],
        crates: vec![
            krate(0, "lib", Origin::Library("lib".to_owned()), Vec::new(), None),
            krate(1, "derive", Origin::Library("derive".to_owned()), Vec::new(), Some(vec![Macro {
                name: "Show".to_owned(),
                kind: Kind::Derive,
            }])),
            krate(2, "app", Origin::Local, vec![dep(0, "lib"), dep(1, "derive")], None),
        ],
        expansions: vec![Expansion {
            call: Call { krate: "derive".to_owned(), name: "Show".to_owned(), input: "struct S ;".to_owned(), attr: None },
            output: Ok("impl S { }".to_owned()),
        }],
    }
}

fn no_builtin(_: &Call) -> Option<Result<String, String>> {
    None
}

fn load(pack: Pack) -> (Db, FileId) {
    let project = pack.project(no_builtin).expect("the pack loads");
    let mut change = ChangeWithProcMacros::default();
    for (id, _, text) in project.files {
        change.change_file(id, Some(text));
    }
    change.set_roots(project.roots);
    change.set_crate_graph(project.graph);
    change.set_proc_macros(project.macros);
    let mut db = Db::for_tests();
    // Setting the crate graph maintains every crate's def map, which reads the proc macros: the
    // input is there first, as `load-cargo` makes it, and `build_in` gives it the pack's.
    ProcMacros::init_default(&mut db, Durability::HIGH);
    change.apply(&mut db);
    (db, project.main)
}

#[test]
fn a_dependency_after_what_depends_on_it_is_refused() {
    let mut pack = pack();
    pack.crates.swap(0, 2);
    assert_eq!(pack.project(no_builtin).err().as_deref(), Some("a dependency after what depends on it"));
}

#[test]
fn a_bad_edition_is_refused() {
    let mut pack = pack();
    pack.crates[0].edition = "1999".to_owned();
    assert_eq!(pack.project(no_builtin).err().as_deref(), Some("a bad edition"));
}

#[test]
fn a_root_with_a_file_not_there_is_refused() {
    let mut pack = pack();
    pack.roots[0].files.push(9);
    assert_eq!(pack.project(no_builtin).err().as_deref(), Some("a root with a file there is not"));
}

#[test]
fn kinds_round_trip() {
    for kind in [Kind::Derive, Kind::Bang, Kind::Attr] {
        assert_eq!(Kind::of(kind.proc_macro_kind()), kind);
    }
}

#[test]
fn a_recorded_pack_is_the_workspace_it_was_loaded_from() {
    let (mut db, main) = load(pack());
    let program = *base_db::all_crates(&db)
        .iter()
        .find(|it| it.data(&db).root_file_id == main)
        .expect("the program's crate");
    let mut crates_seen = 0;
    let again = record(&mut db, program, |_, crates| crates_seen = crates.len()).expect("the pack records");
    assert_eq!(crates_seen, 3);
    let before = pack();
    assert_eq!(again.main, before.main);
    assert_eq!(again.tuple, before.tuple);
    assert_eq!(again.data_layout, before.data_layout);
    let names = |pack: &Pack| pack.crates.iter().map(|it| it.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&again), names(&before));
    let deps = |pack: &Pack| {
        pack.crates.iter().map(|it| it.deps.iter().map(|it| (it.krate, it.name.clone())).collect::<Vec<_>>()).collect::<Vec<_>>()
    };
    assert_eq!(deps(&again), deps(&before));
    let mut texts: Vec<_> = again.files.iter().map(|it| (it.id, it.text.clone())).collect();
    texts.sort();
    let mut want: Vec<_> = before.files.iter().map(|it| (it.id, it.text.clone())).collect();
    want.sort();
    assert_eq!(texts, want);
    let derive = &again.crates[1];
    let kinds: Vec<_> = derive.macros.as_ref().expect("a proc macro crate").iter().map(|it| (it.name.clone(), it.kind)).collect();
    assert_eq!(kinds, [("Show".to_owned(), Kind::Derive)]);
    for (a, b) in again.crates.iter().zip(&before.crates) {
        let mut cfg = a.cfg.clone();
        cfg.sort();
        assert_eq!(cfg, b.cfg, "{:?}", a.name);
        assert_eq!(a.env, b.env, "{:?}", a.name);
    }
}

//! Tests of the maintained name resolution (docs/v2/plan/design/dbsp.md §12), and the helpers
//! they share.
//!
//! A maintained def map is checked against two references in the same database, through the
//! canonical projection of `facts/tests.rs` (`project`, `compare`): the map a fresh maintainer
//! makes of the same inputs (`delta::cold`, the cold run of §3), with nothing left out, and
//! `facts::collect_crate`'s, which may differ only where §6 says ([`DIFFERENCES`]). One database
//! keeps both sides' interned ids (macro calls, macro files, other crates' modules) the same, so
//! the projection compares them exactly; across two databases it could not.

mod alloc;
mod cost;
mod random;
mod trace;

use std::fmt::Write as _;

use base_db::{Crate, Db, Maintained, all_crates};
use blaze_dbsp::Work;
use blaze_mem::Tag;
use hir_expand::{MacroCallKind, fixture, name::Name};
use intern::Symbol;
use span::FileId;

use crate::{
    ModuleId,
    nameres::{
        DefMap, DefMapPair, ModulesSlot, ScopeSlot, Unmaintained, crate_local_def_map,
        diagnostics::DefDiagnosticKind,
        facts::tests::{
            Exempt, compare, differential::ORDER_DEPENDENT, extra_proc_macros,
            new_crate_local_def_map, project,
        },
        frozen,
    },
    test_db::TestDB,
};

/// The crate's def map as a fresh maintainer makes it: maintained from an empty state, without
/// the database's state or published inputs. A query of its own, so the tracked structs it
/// makes live in one.
#[blaze_query::tracked(returns(ref))]
fn cold_crate_local_def_map(db: &Db, krate: Crate) -> DefMapPair<'_> {
    blaze_mem::frame::task(Tag::Nameres, |frame| {
        let (mut def_map, local) = super::cold(db, frame, krate);
        def_map.hand_over_scopes(db);
        DefMapPair::new(db, def_map, local)
    })
}

pub(super) fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

/// A database of a fixture, which maintained every crate as it was loaded, with the fixture's
/// files by path in the fixture's order.
pub(super) struct Loaded {
    pub(super) db: TestDB,
    pub(super) files: Vec<(String, FileId)>,
}

impl Loaded {
    pub(super) fn new(fixture: &str) -> Loaded {
        let fx = fixture::db_with_proc_macros(fixture, extra_proc_macros());
        let files = fx
            .paths()
            .iter()
            .cloned()
            .zip(fx.files().iter().map(|it| it.file_id(fx.db())))
            .collect();
        Loaded { db: fx.into_db(), files }
    }

    pub(super) fn file(&self, path: &str) -> FileId {
        self.files
            .iter()
            .find(|(it, _)| it == path)
            .unwrap_or_else(|| panic!("no file `{path}` in the fixture: {:?}", self.files))
            .1
    }

    pub(super) fn text(&self, file: FileId) -> String {
        self.db.file_text(file).text(&self.db).as_str().to_owned()
    }

    /// The crate whose root is the fixture's first file.
    pub(super) fn krate(&self) -> Crate {
        let root = self.files[0].1;
        *all_crates(&self.db)
            .iter()
            .find(|krate| krate.data(&self.db).root_file_id == root)
            .unwrap_or_else(|| panic!("no crate has `{}` for its root", self.files[0].0))
    }
}

/// The module at `path` (`crate::a::b` as `["a", "b"]`) of `krate`'s maintained map.
pub(super) fn module_at(db: &Db, krate: Crate, path: &[&str]) -> ModuleId {
    let map = crate_local_def_map(db, krate).def_map(db);
    let mut at = map.root;
    for segment in path {
        at = *map.module(db, at).children(db).get(&name(segment)).unwrap_or_else(|| {
            panic!("no module `{segment}` of `crate::{}`", path.join("::"))
        });
    }
    at
}

/// `crate::a::b` for a module of a crate's map.
pub(super) fn module_path(db: &Db, module: ModuleId) -> String {
    let mut names = Vec::new();
    let mut at = module;
    while let Some(parent) = at.containing_module_inside_def_map(db) {
        names.push(at.name(db).map_or_else(|| "?".to_owned(), |it| it.as_str().to_owned()));
        at = parent;
    }
    names.push("crate".to_owned());
    names.reverse();
    names.join("::")
}

/// The maintained map of `krate` without ids: each module's path with the names in its scope
/// and the namespaces each is in, sorted.
pub(super) fn summary(db: &Db, krate: Crate) -> Vec<(String, Vec<String>)> {
    let map = crate_local_def_map(db, krate).def_map(db);
    let mut out: Vec<(String, Vec<String>)> = map
        .modules(db)
        .map(|(id, data)| {
            let mut names: Vec<String> = data
                .scope(db)
                .entries()
                .map(|(name, per_ns)| {
                    format!(
                        "{} types {} values {} macros {}",
                        name.as_str(),
                        per_ns.take_types().is_some(),
                        per_ns.take_values().is_some(),
                        per_ns.take_macros().is_some()
                    )
                })
                .collect();
            names.sort();
            (module_path(db, id), names)
        })
        .collect();
    out.sort();
    out
}

/// Panics naming `what` unless `def_map` is what the maintainer published: its modules a
/// `CrateResolution`, every scope a `ScopeInput`.
pub(super) fn assert_published(db: &Db, def_map: &DefMap, what: &str) {
    assert!(
        matches!(def_map.modules, ModulesSlot::Published(_)),
        "{what}: the modules of crate {:?} are not published: {:?}",
        def_map.krate,
        def_map.modules
    );
    for (id, data) in def_map.modules(db) {
        assert!(
            matches!(data.scope, ScopeSlot::Input(_)),
            "{what}: the scope of {id:?} is not a published input: {:?}",
            data.scope
        );
    }
}

// ---------------------------------------------------------------------------------------------
// §6: where the maintained map may differ from `facts`'. §6 names them against `facts/` and the
// old collector; `facts/` now has rustc's rule for 2 (glob ties by position) and 3 (an import
// through a glob waits for its module's expansions), so there the maintained map equals it.

/// One of design §6's differences from `facts/`.
pub(super) struct Difference {
    /// Its number in §6's list.
    pub(super) number: u8,
    pub(super) name: &'static str,
    /// What rustc (the pinned nightly) says of the program; empty where rustc accepts it and
    /// the difference is one the projection does not see.
    pub(super) rustc: &'static str,
    pub(super) fixture: &'static str,
    /// What the comparison with `facts` leaves out (only 4's: attributes reseeded at once). The
    /// comparison with a fresh maintainer leaves out nothing.
    pub(super) exempt: Exempt,
}

pub(super) const DIFFERENCES: &[Difference] = &[
    Difference {
        number: 1,
        name: "position_order_against_component_order",
        // Accepted: the columns are built in another order, and the projection sorts them.
        rustc: "",
        fixture: r#"
mod late { pub use crate::early::*; pub struct L; }
mod early { pub struct E; pub fn e() {} }
pub use late::*;
pub use early::E as Again;
mod user { pub use crate::late::L; pub use crate::*; }
"#,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 2,
        name: ORDER_DEPENDENT[0].name,
        rustc: ORDER_DEPENDENT[0].rustc,
        fixture: ORDER_DEPENDENT[0].fixture,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 2,
        name: ORDER_DEPENDENT[1].name,
        rustc: ORDER_DEPENDENT[1].rustc,
        fixture: ORDER_DEPENDENT[1].fixture,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 3,
        name: ORDER_DEPENDENT[2].name,
        // Accepted: rustc's glob binding of `foo::T` is undetermined while `foo` has unexpanded
        // macros, so `use foo::T` waits for the expansion and binds its `T`
        // ([`an_import_through_a_glob_waits_for_its_modules_expansions`]).
        rustc: "",
        fixture: ORDER_DEPENDENT[2].fixture,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 4,
        name: ORDER_DEPENDENT[3].name,
        rustc: ORDER_DEPENDENT[3].rustc,
        fixture: ORDER_DEPENDENT[3].fixture,
        exempt: ORDER_DEPENDENT[3].exempt,
    },
];

/// What the comparison of fixture `name` with `facts` leaves out: its §6 difference's.
pub(super) fn facts_exempt(name: &str) -> Exempt {
    DIFFERENCES.iter().find(|it| it.name == name).map_or(Exempt::NONE, |it| it.exempt)
}

/// Whether `def_map` has an attribute left unresolved: §6's 4, the one difference a program an
/// edit script made may show.
fn unresolved_attribute(db: &Db, def_map: &DefMap) -> bool {
    def_map.records(db).diagnostics.iter().any(|it| {
        matches!(&it.kind, DefDiagnosticKind::UnresolvedMacroCall { ast: MacroCallKind::Attr { .. }, .. })
    })
}

/// Compares every maintained crate of `db` with a fresh maintainer's map (nothing left out)
/// and with `facts`' (leaving out `exempt`, and with `rejected` also 4's difference where either
/// map has an [`unresolved_attribute`]: for a program an edit script made, where no fixture
/// says), appending each difference to `out` under `what`. A crate served by a frozen image is
/// not maintained and is skipped.
pub(super) fn check_db(db: &Db, what: &str, exempt: &Exempt, rejected: bool, out: &mut String) {
    for &krate in all_crates(db).iter() {
        let pair = crate_local_def_map(db, krate);
        let maintained = pair.def_map(db);
        if !matches!(maintained.modules, ModulesSlot::Published(_)) {
            assert!(
                frozen::served(db, krate).is_some(),
                "{what}: crate {krate:?} is neither maintained nor frozen"
            );
            continue;
        }
        assert_published(db, maintained, what);
        let display = &krate.extra_data(db).display_name;

        let cold = cold_crate_local_def_map(db, krate);
        let diff = compare(
            &project(db, cold.def_map(db), Some(cold.local(db)), &Exempt::NONE),
            &project(db, maintained, Some(pair.local(db)), &Exempt::NONE),
        );
        if !diff.is_empty() {
            _ = writeln!(
                out,
                "== {what}: crate {display:?}: a fresh maintainer's map (old) and the maintained \
                 one (new) differ\n{diff}"
            );
        }

        let facts = new_crate_local_def_map(db, krate);
        let mut exempt = *exempt;
        if rejected {
            exempt.unresolved_imports |=
                unresolved_attribute(db, maintained) || unresolved_attribute(db, facts.def_map(db));
        }
        let diff = compare(
            &project(db, facts.def_map(db), Some(facts.local(db)), &exempt),
            &project(db, maintained, Some(pair.local(db)), &exempt),
        );
        if !diff.is_empty() {
            _ = writeln!(
                out,
                "== {what}: crate {display:?}: `facts::collect_crate`'s map (old) and the \
                 maintained one (new) differ\n{diff}"
            );
        }
    }
}

/// The `nameres` maintainer's records of `krate` kept since the last take, one per write.
pub(super) fn take_work(db: &mut Db, krate: Crate) -> Vec<Work> {
    let mut records: Vec<Maintained> = Vec::new();
    db.take_maintained(&mut records);
    let mut out = Vec::new();
    for Maintained { write, maintainer, krate: of, work } in records {
        if maintainer != "nameres" || of != krate {
            continue;
        }
        assert_eq!(write as usize, out.len(), "one `nameres` record of {krate:?} per write");
        out.push(work);
    }
    out
}

/// The `Work` of the one write `f` makes to `db`, for `krate`.
pub(super) fn work_of(db: &mut Db, krate: Crate, f: impl FnOnce(&mut Db)) -> Work {
    db.keep_maintained(true);
    let mut stale = Vec::new();
    db.take_maintained(&mut stale);
    f(db);
    let work = take_work(db, krate);
    db.keep_maintained(false);
    assert_eq!(work.len(), 1, "the edit is one write: {work:?}");
    work[0]
}

#[test]
fn the_differences_are_named_and_agree_on_everything_else() {
    let mut numbers: Vec<u8> = DIFFERENCES.iter().map(|it| it.number).collect();
    numbers.dedup();
    assert_eq!(numbers, [1, 2, 3, 4], "each of §6's differences has a fixture");
    let mut failures = String::new();
    for it in DIFFERENCES {
        assert!(
            matches!(it.number, 1 | 3) || it.rustc.starts_with("error"),
            "{}: say what rustc says of it",
            it.name
        );
        let loaded = Loaded::new(it.fixture);
        check_db(&loaded.db, it.name, &it.exempt, false, &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

/// Of two globs bringing one name, the maintained binding is the first by position, also when
/// it is the second to arrive (§6 "glob ties").
#[test]
fn a_glob_tie_follows_position_not_arrival() {
    let fixture = r#"
//- /lib.rs
mod a;
mod b;
use b::*;
pub use S as Exported;
//- /a.rs
pub struct S;
//- /b.rs
pub struct S;
"#;
    let mut loaded = Loaded::new(fixture);
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    // The module whose `S` the root's `S` is.
    let container = |db: &Db| {
        let map = crate_local_def_map(db, krate).def_map(db);
        let (def, _) = map.module(db, map.root).scope(db).type_(&name("S")).expect("`S` is in scope");
        let crate::ModuleDefId::AdtId(crate::AdtId::StructId(it)) = def else { panic!("`S` is {def:?}") };
        crate::Lookup::lookup(&it, db).container.name(db)
    };
    assert_eq!(container(&loaded.db), Some(name("b")), "one glob: `b`'s `S`");
    loaded.db.set_file_text(root, "mod a;\nmod b;\nuse a::*;\nuse b::*;\npub use S as Exported;\n");
    assert_eq!(container(&loaded.db), Some(name("a")), "`a`'s glob arrived second and comes first");
    let mut failures = String::new();
    check_db(&loaded.db, "glob tie", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
    loaded.db.set_file_text(root, "mod a;\nmod b;\nuse b::*;\npub use S as Exported;\n");
    assert_eq!(container(&loaded.db), Some(name("b")), "`a`'s glob gone again");
}

/// `use foo::T` where `foo` globs `x::T` and a macro in `foo` declares its own `T`: rustc binds
/// the expanded one, as its glob binding is undetermined while `foo` has unexpanded macros
/// (`rustc_resolve::ident`, `resolve_ident_in_module_globs_unadjusted`). A cold map and one
/// maintained through an edit that reruns only the root agree, with each other and with rustc
/// (§6's third difference).
#[test]
fn an_import_through_a_glob_waits_for_its_modules_expansions() {
    let mut loaded = Loaded::new(ORDER_DEPENDENT[2].fixture);
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    let container = |db: &Db| {
        let map = crate_local_def_map(db, krate).def_map(db);
        let (def, _) = map.module(db, map.root).scope(db).type_(&name("T")).expect("`T` is in scope");
        let crate::ModuleDefId::AdtId(crate::AdtId::StructId(it)) = def else { panic!("`T` is {def:?}") };
        crate::Lookup::lookup(&it, db).container.name(db)
    };
    assert_eq!(container(&loaded.db), Some(name("foo")), "loaded: `foo`'s expanded `T`");
    let text = format!("{}fn edited() {{}}\n", loaded.text(root));
    loaded.db.set_file_text(root, &text);
    assert_eq!(container(&loaded.db), Some(name("foo")), "the root rerun alone: `foo`'s expanded `T`");
    let mut failures = String::new();
    check_db(&loaded.db, "an import through a glob", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// The crate attributes of the root are the maintained map's crate data with no group's ops
/// changed: an edit that removes a `#![feature]` removes it from the map, as a fresh run has it
/// (`random::batched_edits_over_the_fixtures`, the `sanitize` fixture).
#[test]
fn removing_a_crate_feature_removes_it_from_the_maintained_map() {
    let mut loaded = Loaded::new("//- /lib.rs crate:foo crate-attr:no_std\n#![feature(sanitize)]\nfn f() {}\n");
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    let sanitize = Symbol::intern("sanitize");
    let enabled = |db: &Db| crate_local_def_map(db, krate).def_map(db).features().is_enabled(&sanitize);
    assert!(enabled(&loaded.db), "loaded: `sanitize` is enabled");
    loaded.db.set_file_text(root, "fn f() {}\n");
    assert!(!enabled(&loaded.db), "the feature is gone with its attribute");
    let mut failures = String::new();
    check_db(&loaded.db, "a crate feature removed", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// One write that adds an item to the root and removes a child of another module: the second
/// file's changed children make the read walk the tree, which still sees the root's new log.
#[test]
fn a_write_that_walks_keeps_the_logs_it_compared_before() {
    let mut loaded =
        Loaded::new("//- /lib.rs\nmod foo;\n//- /foo/mod.rs\npub mod bar;\n//- /foo/bar.rs\npub struct B;\n");
    let (lib, foo) = (loaded.file("/lib.rs"), loaded.file("/foo/mod.rs"));
    loaded.db.batch(|db| {
        db.set_file_text(lib, "pub struct S;\nmod foo;\n");
        db.set_file_text(foo, "");
    });
    let krate = loaded.krate();
    let map = crate_local_def_map(&loaded.db, krate).def_map(&loaded.db);
    assert!(map.module(&loaded.db, map.root).scope(&loaded.db).get(&name("S")).take_types().is_some(), "`S` is in");
    let mut failures = String::new();
    check_db(&loaded.db, "a write that walks", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A variant renamed is no binding of its enum's module: a glob of the enum, in another crate
/// or in this one, still takes the new name.
#[test]
fn renaming_a_variant_reaches_the_globs_of_its_enum() {
    let mut loaded = Loaded::new(
        "//- /main.rs crate:main deps:dep\nuse dep::Foo::*;\n//- /lib.rs crate:dep\npub enum Foo { Bar, Baz }\npub mod m { pub use super::Foo::*; }\n",
    );
    let lib = loaded.file("/lib.rs");
    loaded.db.set_file_text(lib, "pub enum Foo { S, Baz }\npub mod m { pub use super::Foo::*; }\n");
    let mut failures = String::new();
    check_db(&loaded.db, "a variant renamed", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// An edit of a dependency's macro body that keeps the macro's id changes no binding, and
/// still changes what a call of it in this crate expands to.
#[test]
fn editing_a_dependencys_macro_body_expands_its_calls_again() {
    const STD: &str = "pub mod prelude {\n    pub mod rust_2018 {\n        pub macro global_asm() {\n            pub struct S;\n        }\n    }\n}\n";
    let mut loaded = Loaded::new(&format!(
        "//- /lib.rs edition:2018 crate:lib deps:std\nglobal_asm!();\n//- /std.rs crate:std\n{STD}"
    ));
    let std = loaded.file("/std.rs");
    let lib = loaded.file("/lib.rs");
    let lib = *all_crates(&loaded.db).iter().find(|it| it.data(&loaded.db).root_file_id == lib).expect("`lib`");
    let has_s = |db: &Db| {
        let map = crate_local_def_map(db, lib).def_map(db);
        map.module(db, map.root).scope(db).get(&name("S")).take_types().is_some()
    };
    assert!(has_s(&loaded.db), "loaded: the call declares `S`");
    loaded.db.set_file_text(std, &STD.replace("            pub struct S;\n", ""));
    assert!(!has_s(&loaded.db), "the call expands to nothing now");
    let mut failures = String::new();
    check_db(&loaded.db, "a dependency's macro body edited", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// `foo` is made by a dependency's `mk_foo!()`, so its definition is in a macro file of the
/// call's own crate: the file a call of `foo` reads it from is the dependency's, up the chain.
#[test]
fn editing_the_body_of_a_macro_a_dependencys_macro_made_expands_its_calls_again() {
    const LIB: &str = "#[macro_export]\nmacro_rules! mk_foo {\n    () => {\n        macro_rules! foo {\n            () => { struct Ok; }\n        }\n    }\n}\n";
    let mut loaded = Loaded::new(&format!(
        "//- /main.rs crate:main deps:lib\n#[macro_use]\nextern crate lib;\nmk_foo!();\nmod a {{\n    foo!();\n}}\n//- /lib.rs crate:lib\n{LIB}"
    ));
    let main = loaded.krate();
    let lib = loaded.file("/lib.rs");
    let has = |db: &Db, it: &str| {
        let a = module_at(db, main, &["a"]);
        let map = crate_local_def_map(db, main).def_map(db);
        map.module(db, a).scope(db).get(&name(it)).take_types().is_some()
    };
    assert!(has(&loaded.db, "Ok"), "loaded: `foo!()` declares `Ok`");
    loaded.db.set_file_text(lib, &LIB.replace("struct Ok;", "struct Fine;"));
    assert!(!has(&loaded.db, "Ok") && has(&loaded.db, "Fine"), "`foo!()` expands to the old body");
    let mut failures = String::new();
    check_db(&loaded.db, "a macro-made macro's body edited", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A `mod` whose file is configured out by its `#![cfg]` has no module, so no group holds the
/// file: a write of the file alone that drops the `#![cfg]` still brings the module in.
#[test]
fn removing_a_mod_files_cfg_brings_the_module_in() {
    let mut loaded = Loaded::new("//- /main.rs\nmod module;\n//- /module.rs\n#![cfg(NEVER)]\nstruct S;\n");
    let file = loaded.file("/module.rs");
    loaded.db.set_file_text(file, "struct S;\n");
    let krate = loaded.krate();
    assert_eq!(module_path(&loaded.db, module_at(&loaded.db, krate, &["module"])), "crate::module");
    let mut failures = String::new();
    check_db(&loaded.db, "a mod file's cfg removed", &Exempt::NONE, false, &mut failures);
    loaded.db.set_file_text(file, "#![cfg(NEVER)]\nstruct S;\n");
    check_db(&loaded.db, "a mod file's cfg back", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// Two `mod`s of one name in one parent (E0428) are one module id: the first keeps the name,
/// on load and after an edit adds the second.
#[test]
fn a_second_mod_of_a_name_declares_nothing() {
    let mut loaded = Loaded::new("//- /lib.rs\npub mod a { pub struct S; }\n//- /a.rs\npub struct T;\n");
    let root = loaded.file("/lib.rs");
    loaded.db.set_file_text(root, "pub mod a { pub struct S; }\npub mod a;\n");
    let krate = loaded.krate();
    let a = module_at(&loaded.db, krate, &["a"]);
    let map = crate_local_def_map(&loaded.db, krate).def_map(&loaded.db);
    let scope = map.module(&loaded.db, a).scope(&loaded.db);
    assert!(scope.get(&name("S")).take_types().is_some(), "the first `mod a` keeps the name");
    assert!(scope.get(&name("T")).take_types().is_none(), "the second declares nothing");
    let mut failures = String::new();
    check_db(&loaded.db, "a second mod of a name", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A glob of a module with an unresolved import takes no name of it, whether the glob shares a
/// run with the import (the maintainer's region) or reads it from upstream (`facts`' components).
#[test]
fn a_glob_carries_no_unresolved_name() {
    const LIB: &str = "mod foo;\nuse crate::foo::E;\n";
    let mut loaded = Loaded::new(&format!("//- /lib.rs\n{LIB}//- /foo.rs\nenum E {{}}\n"));
    let root = loaded.file("/lib.rs");
    loaded.db.set_file_text(root, &format!("pub mod m {{ pub use super::*; }}\n{LIB}"));
    let krate = loaded.krate();
    let map = crate_local_def_map(&loaded.db, krate).def_map(&loaded.db);
    let m = module_at(&loaded.db, krate, &["m"]);
    let e = name("E");
    assert!(
        !map.module(&loaded.db, m).scope(&loaded.db).resolutions().any(|(it, _)| it == Some(e)),
        "`m` has no `E` of the failed import"
    );
    let mut failures = String::new();
    check_db(&loaded.db, "a glob of a failed import", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A crate's injected std prelude is a dependency's module, named in its root's round-0 log:
/// an edit of the dependency alone that renames the module away takes it out of the crate's
/// map, and renaming it back brings it in again.
#[test]
fn a_dependencys_prelude_renamed_away_leaves_the_maintained_map() {
    const CORE: &str = "pub mod prelude {\n    pub mod rust_2018 {\n        pub struct Rust;\n    }\n}\n";
    let mut loaded = Loaded::new(&format!(
        "//- /main.rs edition:2018 crate:main deps:core\n#![no_std]\nuse Rust;\n//- /core.rs crate:core\n{CORE}"
    ));
    let core = loaded.file("/core.rs");
    let main = loaded.file("/main.rs");
    let main =
        *all_crates(&loaded.db).iter().find(|it| it.data(&loaded.db).root_file_id == main).expect("`main`");
    let prelude = |db: &Db| crate_local_def_map(db, main).def_map(db).prelude();
    assert!(prelude(&loaded.db).is_some(), "loaded: `core::prelude::rust_2018` is the prelude");
    loaded.db.set_file_text(core, &CORE.replacen("prelude", "S", 1));
    assert_eq!(prelude(&loaded.db), None, "the prelude is gone with its module");
    let mut failures = String::new();
    check_db(&loaded.db, "a dependency's prelude renamed away", &Exempt::NONE, false, &mut failures);
    loaded.db.set_file_text(core, CORE);
    assert!(prelude(&loaded.db).is_some(), "the prelude is back with its module");
    check_db(&loaded.db, "a dependency's prelude renamed back", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A module's id outlives the revisions that never read it: the maintainer publishes each
/// crate in a write of its own, so a crate maintained early goes unread for as many revisions
/// as there are crates after it, while their scopes hold its modules' ids. Were
/// `child_module`'s interned arguments collected, the module would be deleted and its id given
/// to a module of a later crate (hello's std build: `alloc`'s `string`, read through
/// `rustc_std_workspace_alloc`, became a module of `gimli`, whose own maintenance then asked
/// for its unpublished map).
#[test]
fn a_dependencys_module_ids_outlive_the_crates_maintained_after_it() {
    let mut fixture = String::from("//- /lib.rs crate:lib\npub mod string { pub struct S; }\n");
    for krate in 0..8 {
        _ = writeln!(fixture, "//- /c{krate}.rs crate:c{krate}");
        for module in 0..64 {
            _ = writeln!(fixture, "pub mod m{module} {{ pub struct T; }}");
        }
    }
    fixture.push_str("//- /main.rs crate:main deps:lib\npub use lib::string::S;\n");
    let loaded = Loaded::new(&fixture);
    let db = &loaded.db;
    let crate_of = |root: &str| {
        let file = loaded.file(root);
        *all_crates(db).iter().find(|it| it.data(db).root_file_id == file).expect("a crate of the root")
    };
    let (lib, main) = (crate_of("/lib.rs"), crate_of("/main.rs"));
    let string = module_at(db, lib, &["string"]);
    assert_eq!(string.krate(db), lib, "`lib::string`'s id names a module of another crate");
    assert_eq!(module_path(db, string), "crate::string");
    let map = crate_local_def_map(db, main).def_map(db);
    let (def, _) = map.module(db, map.root).scope(db).type_(&name("S")).expect("`S` is imported");
    let crate::ModuleDefId::AdtId(crate::AdtId::StructId(it)) = def else { panic!("`S` is {def:?}") };
    assert_eq!(crate::Lookup::lookup(&it, db).container, string, "`S` is `lib::string::S`");
    let mut failures = String::new();
    check_db(db, "a dependency's module ids", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A crate the maintainer does not go over (here, one taken out of the crate graph) has no
/// resolution: asking for its def map stops with an [`Unmaintained`] naming it, and resolves
/// nothing in its place.
#[test]
fn a_crate_with_no_maintained_resolution_is_an_error_naming_it() {
    let mut loaded = Loaded::new(
        r#"
//- /main.rs crate:main
pub struct M;
//- /lib.rs crate:lib
pub struct L;
"#,
    );
    let main = loaded.krate();
    let lib = *all_crates(&loaded.db).iter().find(|it| **it != main).expect("the crate `lib`");
    let map = crate_local_def_map(&loaded.db, lib).def_map(&loaded.db);
    assert!(map.module(&loaded.db, map.root).scope(&loaded.db).type_(&name("L")).is_some(), "`lib` is maintained");
    base_db::set_all_crates_with_durability(&mut loaded.db, [main], blaze_query::db::Durability::LOW);
    let db = &loaded.db;
    let map = crate_local_def_map(db, main).def_map(db);
    assert!(map.module(db, map.root).scope(db).type_(&name("M")).is_some(), "`main` is still maintained");
    let caught = Unmaintained::catch(std::panic::AssertUnwindSafe(|| {
        _ = crate_local_def_map(db, lib);
    }));
    let Err(error) = caught else { panic!("`lib`'s def map was given with no maintained resolution") };
    assert_eq!(error, Unmaintained { krate: lib, name: Some("lib".to_owned()), asking: None });
    let text = error.to_string();
    assert!(text.contains("`lib`") && text.contains("no maintainer ran"), "{text}");
}

/// A `std` cargo passes `noprelude` (build-std's `--extern noprelude:std`) is in no extern
/// prelude until the `extern crate std;` rustc injects puts it there, in every edition; the
/// std prelude is then `std`'s, as for any crate that depends on `std`.
#[test]
fn a_noprelude_std_is_put_in_the_extern_prelude_by_its_injected_extern_crate() {
    let loaded = Loaded::new(
        r#"
//- /main.rs crate:main edition:2021 deps:std extern-prelude:
use std::process::Command;
//- /std.rs crate:std
pub mod process { pub struct Command; }
pub mod prelude {
    pub mod v1 { pub enum Option<T> { None, Some(T) } }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
"#,
    );
    let main = loaded.krate();
    let db = &loaded.db;
    let std = *all_crates(db).iter().find(|it| **it != main).expect("the crate `std`");
    let pair = crate_local_def_map(db, main);
    assert!(
        pair.local(db).extern_prelude().any(|(it, _)| *it == name("std")),
        "`std` is not in the extern prelude"
    );
    let map = pair.def_map(db);
    let (prelude, _) = map.prelude().expect("the std prelude is injected");
    assert_eq!(prelude.krate(db), std);
    assert_eq!(module_path(db, prelude), "crate::prelude::rust_2021");
    assert!(map.module(db, map.root).scope(db).type_(&name("Command")).is_some(), "`Command` is imported");
    let mut failures = String::new();
    check_db(db, "a noprelude std", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// A `core` cargo passes `noprelude` beside `std` is in the extern prelude too, as rustc's
/// resolver puts `core` there for every crate but a `no_core` one: anyhow's build probe, an
/// edition 2018 crate, starts with `use core::error::{self, Error};`.
#[test]
fn a_noprelude_core_is_in_the_extern_prelude_beside_std() {
    let loaded = Loaded::new(
        r#"
//- /main.rs crate:main edition:2018 deps:std,core extern-prelude:
use core::error::Error;
//- /std.rs crate:std deps:core
pub mod prelude {
    pub mod v1 { pub enum Option<T> { None, Some(T) } }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
//- /core.rs crate:core
pub mod error { pub trait Error {} }
"#,
    );
    let main = loaded.krate();
    let db = &loaded.db;
    let pair = crate_local_def_map(db, main);
    let prelude: Vec<String> =
        pair.local(db).extern_prelude().map(|(it, _)| it.as_str().to_owned()).collect();
    assert_eq!(prelude, ["core", "std"], "rustc's order: `core`, then `std`");
    let map = pair.def_map(db);
    assert!(map.module(db, map.root).scope(db).type_(&name("Error")).is_some(), "`Error` is imported");
    let mut failures = String::new();
    check_db(db, "a noprelude core", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// The text of `foo/bar.rs` in [`macro_heavy`], `body` the body of its first fn.
fn macro_heavy_text(body: &str) -> String {
    format!(
        "
fn f() {{{body}}}

m!(X);
macro_rules! m2 {{
    ($ident:ident) => {{
        fn f2() {{
            $ident + $ident;
        }};
    }}
}}
m2!(X);

#[proc_macros::identity]
#[derive(proc_macros::DeriveIdentity)]
pub struct S {{}}
"
    )
}

/// The macro-heavy fixture of `typing_inside_macro_heavy_file_should_not_invalidate_def_map`,
/// its crate's def map read: the database, `foo/bar.rs` and the crate.
fn macro_heavy() -> (TestDB, FileId, Crate) {
    let fx = fixture::db(&format!(
        r"
//- proc_macros: identity, derive_identity
//- /lib.rs
macro_rules! m {{
    ($ident:ident) => {{
        fn fm() {{
            $ident + $ident;
        }};
    }}
}}
mod foo;

//- /foo/mod.rs
pub mod bar;

//- /foo/bar.rs
{}",
        macro_heavy_text("")
    ));
    let file = fx.files().last().expect("the fixture's files").file_id(fx.db());
    let db = fx.into_db();
    let krate = crate::test_db::TestDBExt::fetch_test_crate(&db);
    crate_local_def_map(&db, krate);
    (db, file, krate)
}

#[test]
fn an_edit_in_a_body_of_a_macro_heavy_file_sets_nothing_the_def_map_reads() {
    let (mut db, file, krate) = macro_heavy();
    let before = super::publish::Slices::of(&db, krate);
    let extra = krate.extra_data(&db).clone();

    let work = work_of(&mut db, krate, |db| db.set_file_text(file, &macro_heavy_text("0")));
    assert_eq!(work.slices, 0, "inputs the maintainer set, though each ends as it was: {work:?}");
    let after = super::publish::Slices::of(&db, krate);
    assert_eq!(before.differing(&after), Vec::<String>::new(), "fields of the crate's resolution set");
    assert_eq!(*krate.extra_data(&db), extra, "the crate's extra data");
    let log = crate::test_db::TestDBExt::executed_in_read_order(&db, || _ = crate_local_def_map(&db, krate));
    assert!(log.iter().all(|it| it != "resolution_of" && it != "crate_local_def_map"), "{log:#?}");
}

/// Where `resolution_of` runs again after a body edit that sets nothing it reads: in the write
/// (the maintainer), in a read after it, or in a read made in the write's closure, as
/// `check_def_map_is_not_recomputed` reads.
#[test]
fn an_edit_in_a_body_of_a_macro_heavy_file_runs_no_resolution_of_again() {
    use crate::test_db::TestDBExt;
    let ran = |log: &[String]| log.iter().filter(|it| *it == "resolution_of").count();

    // The first write after the load runs the `proc_macros` crate's `resolution_of` once, to
    // fill its memo; each edit is measured after one such write.
    let warmed = || {
        let (mut db, file, krate) = macro_heavy();
        db.set_file_text(file, &macro_heavy_text("0"));
        _ = crate_local_def_map(&db, krate);
        (db, file, krate)
    };
    let (mut db, file, krate) = warmed();
    let write = db.executed_in_write(|db| db.set_file_text(file, &macro_heavy_text("1")));
    let read = db.executed_in_read_order(|| _ = crate_local_def_map(&db, krate));
    let second = db.executed_in_write(|db| db.set_file_text(file, &macro_heavy_text("2")));

    let (mut db, file, krate) = warmed();
    let within = db.executed_in_write(|db| {
        db.set_file_text(file, &macro_heavy_text("1"));
        _ = crate_local_def_map(db, krate);
    });

    let (mut db, file, _) = warmed();
    let events = db.log_write_in_read_order(|db| db.set_file_text(file, &macro_heavy_text("1")));
    let storage = blaze_query::db::Database::storage(&db);
    let of: Vec<String> = events
        .iter()
        .filter_map(|event| match event.kind {
            blaze_query::db::EventKind::WillExecute { database_key }
                if storage.ingredient_name(database_key.ingredient_index()) == "resolution_of" =>
            {
                let key = database_key.key_index();
                let krate = all_crates(&db).iter().copied().find(|it| blaze_query::db::AsId::as_id(it) == key);
                Some(krate.map_or_else(
                    || format!("{key:?}, no crate"),
                    |it| format!("{:?}", it.extra_data(&db).display_name),
                ))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        (ran(&write), ran(&read), ran(&within)),
        (0, 0, 0),
        "the crates whose `resolution_of` ran in the write: {of:?}; in a second write: {}\n\
         `resolution_of` ran (in the write, in a read after it, in a read in the write)\n\
         write: {write:#?}\nread: {read:#?}\nwithin: {within:#?}",
        ran(&second)
    );
}

/// An attribute macro's input is its whole item: an edit inside an attributed inline module,
/// which leaves the call where it was, expands the attribute again, and the module gains what
/// the edit wrote. The incremental tests' `typing_inside_an_attribute_should_not_invalidate_def_map`
/// shows no expansion after such an edit (queue 01309).
#[test]
fn an_edit_inside_an_attributed_module_expands_the_attribute_again() {
    let mut loaded = Loaded::new(
        r#"
//- proc_macros: identity
//- /lib.rs
#[proc_macros::identity]
pub mod m {
    pub struct A;
}
"#,
    );
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    let has = |db: &Db, it: &str| {
        let module = module_at(db, krate, &["m"]);
        module.def_map(db).module(db, module).scope(db).type_(&name(it)).is_some()
    };
    assert!(has(&loaded.db, "A") && !has(&loaded.db, "B"), "`m` holds `A` alone");
    let text = loaded.text(root).replace("pub struct A;", "pub struct A;\n    pub struct B;");
    loaded.db.set_file_text(root, &text);
    assert!(has(&loaded.db, "B"), "`m` holds the `B` the edit wrote inside the attributed module");
    let mut failures = String::new();
    check_db(&loaded.db, "an edit inside an attributed module", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// An attribute macro's input is its whole item, the fn body too, and its output may depend on
/// that body: an edit that makes the body of a `#[identity_when_valid]` fn invalid turns its
/// expansion into an error, which a fresh map shows. The item tree leaves bodies out, so this
/// write changes none of the module's round-0 ops (queue 01309,
/// `typing_inside_an_attribute_should_not_invalidate_def_map`).
#[test]
fn an_edit_in_the_body_of_an_attributed_fn_expands_the_attribute_again() {
    let mut loaded = Loaded::new(
        r#"
//- proc_macros: identity_when_valid
//- /lib.rs
#[proc_macros::identity_when_valid]
pub fn f() {}
"#,
    );
    let root = loaded.file("/lib.rs");
    let mut failures = String::new();
    check_db(&loaded.db, "before the edit", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
    loaded.db.set_file_text(root, "#[proc_macros::identity_when_valid]\npub fn f() { let = ; }\n");
    check_db(&loaded.db, "a body the attribute rejects", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// An item with `#[derive]` first waits at round 0 behind the derive's directive, and its
/// collection reads the rest of it later: an edit of its later attributes or its visibility,
/// which keeps its ast id, changes its directive and so the round-0 log, and the maintained map
/// follows it. An edit of a body leaves the log as it was (`random`, `coerce_pointee_errors`).
#[test]
fn an_attribute_or_visibility_edit_of_a_deferred_item_changes_the_round_0_log() {
    use crate::nameres::facts::{
        ops::{DeclOp, MacroOp, MacroOpKind},
        queries,
    };
    let mut loaded = Loaded::new("//- minicore: derive, clone\n//- /lib.rs\n#[derive(Clone)]\n#[repr(C)]\nstruct S;\nfn f() {}\n");
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    let directives = |db: &Db| -> Vec<MacroOp> {
        let ops = &queries::module_decls(db, queries::root_decl(db, krate)).ops;
        // clone: a test's copy of the directives, to compare across a write
        ops.iter().filter_map(|op| if let DeclOp::Macro(it) = op { Some(it.clone()) } else { None }).collect()
    };
    let first = directives(&loaded.db);
    assert!(matches!(&first[..], [MacroOp { kind: MacroOpKind::Attr { .. }, .. }]), "loaded: {first:?}");
    let mut failures = String::new();
    for (what, text, changes) in [
        ("a later attribute", "#[derive(Clone)]\n#[S]\nstruct S;\nfn f() {}\n", true),
        ("the visibility", "#[derive(Clone)]\n#[S]\npub struct S;\nfn f() {}\n", true),
        ("a body", "#[derive(Clone)]\n#[S]\npub struct S;\nfn f() { let _ = 1; }\n", false),
    ] {
        let before = directives(&loaded.db);
        loaded.db.set_file_text(root, text);
        let after = directives(&loaded.db);
        assert_eq!(after != before, changes, "{what}: the log changed: {}\nbefore {before:?}\nafter {after:?}", after != before);
        check_db(&loaded.db, what, &Exempt::NONE, true, &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

/// A `macro_rules!` of a `#[macro_use]` file module inside an inline `#[macro_use]` module
/// reaches the crate root's textual scope and the scope a later sibling file module starts with;
/// removing it from the file takes it out of both (`random`, generated 0x7d5e).
#[test]
fn a_macro_removed_from_a_nested_macro_use_file_leaves_the_textual_scopes() {
    let mut loaded = Loaded::new(
        "//- /lib.rs\n#[macro_use]\npub mod a {\n#[macro_use]\npub mod b;\n}\npub mod b;\n\
         //- /a/b.rs\nmacro_rules! m1 { () => {} }\n//- /b.rs\npub struct T;\n",
    );
    let krate = loaded.krate();
    let has_m1 = |db: &Db, path: &[&str]| {
        let module = module_at(db, krate, path);
        let map = crate_local_def_map(db, krate).def_map(db);
        map.module(db, module).scope(db).get_legacy_macro(&name("m1")).is_some()
    };
    let paths: [&[&str]; 3] = [&[], &["a"], &["b"]];
    for path in paths {
        assert!(has_m1(&loaded.db, path), "loaded: `m1` is not in the textual scope of {path:?}");
    }
    let file = loaded.file("/a/b.rs");
    loaded.db.set_file_text(file, "");
    for path in paths {
        assert!(!has_m1(&loaded.db, path), "removed: `m1` is still in the textual scope of {path:?}");
    }
    let mut failures = String::new();
    check_db(&loaded.db, "a nested macro_use macro removed", &Exempt::NONE, true, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// An import of a dependency's trait item that a macro in the trait makes, by name or by a glob
/// of the trait, goes unresolved when the macro goes: the trait's items are no binding of its
/// module, so the import's read wakes on the trait (`random`,
/// `trait_item_imports_differing_crate#0`).
#[test]
fn removing_the_macro_that_makes_a_dependencys_trait_item_unresolves_its_import() {
    const TRAIT: &str = "pub trait Trait {\n    fn method(&self);\n    m!(MACRO_CONST);\n}\n";
    const MACRO: &str = "macro_rules! m {\n    ($name:ident) => { const $name: () = (); };\n}\n";
    let mut failures = String::new();
    for import in ["use lib::Trait::{method, MACRO_CONST};", "use lib::Trait::*;"] {
        let mut loaded = Loaded::new(&format!(
            "//- /main.rs deps:lib crate:main\n{import}\n//- /lib.rs crate:lib\n{MACRO}{TRAIT}"
        ));
        let main = loaded.krate();
        let has_const = |db: &Db| {
            let root = module_at(db, main, &[]);
            let map = crate_local_def_map(db, main).def_map(db);
            map.module(db, root).scope(db).get(&name("MACRO_CONST")).take_values().is_some()
        };
        assert!(has_const(&loaded.db), "{import}: loaded: `MACRO_CONST` is not imported");
        let lib = loaded.file("/lib.rs");
        loaded.db.set_file_text(lib, TRAIT);
        assert!(!has_const(&loaded.db), "{import}: the macro removed: `MACRO_CONST` is still imported");
        check_db(&loaded.db, import, &Exempt::NONE, true, &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn a_body_edit_beside_a_globbed_enum_runs_no_group_and_a_new_variant_is_imported() {
    let text = |variants: &str, body: &str| {
        format!("enum E {{ {variants} }}\nuse E::*;\nfn f() -> i32 {{ {body} }}\n")
    };
    let mut loaded = Loaded::new(&format!("//- /main.rs crate:main\n{}", text("A, B", "1")));
    let main = loaded.krate();
    let file = loaded.file("/main.rs");
    // The first touch of the enum wakes its glob: what it last woke on is not known yet.
    loaded.db.set_file_text(file, &text("A, B", "2"));
    let work = work_of(&mut loaded.db, main, |db| db.set_file_text(file, &text("A, B", "3")));
    assert_eq!(work.region, 0, "a body edit with the variants as they were ran groups: {work:?}");
    let work = work_of(&mut loaded.db, main, |db| db.set_file_text(file, &text("A, B, C", "3")));
    assert!(work.region > 0, "a new variant woke no glob of its enum: {work:?}");
    let root = module_at(&loaded.db, main, &[]);
    let map = crate_local_def_map(&loaded.db, main).def_map(&loaded.db);
    assert!(map.module(&loaded.db, root).scope(&loaded.db).get(&name("C")).take_types().is_some(), "`C` is not imported");
    let mut failures = String::new();
    check_db(&loaded.db, "a new variant", &Exempt::NONE, true, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn removing_a_globbed_enum_reads_no_variants_of_it_and_unbinds_them() {
    let mut loaded = Loaded::new("//- /main.rs crate:main\nenum E { A, B }\nuse E::*;\nfn f() {}\n");
    let main = loaded.krate();
    let file = loaded.file("/main.rs");
    // A touch first, so the enum's variants are the ones its glob last woke on.
    loaded.db.set_file_text(file, "enum E { A, B }\nuse E::*;\nfn f() { () }\n");
    loaded.db.set_file_text(file, "use E::*;\nfn f() { () }\n");
    let root = module_at(&loaded.db, main, &[]);
    let map = crate_local_def_map(&loaded.db, main).def_map(&loaded.db);
    assert!(map.module(&loaded.db, root).scope(&loaded.db).get(&name("A")).is_none(), "`A` is still imported");
    let mut failures = String::new();
    check_db(&loaded.db, "a globbed enum removed", &Exempt::NONE, true, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn removing_an_item_of_a_trait_or_enum_unresolves_its_import_in_the_same_module() {
    const FEATURE: &str = "#![feature(import_trait_associated_functions)]\n";
    let mut failures = String::new();
    for (import, item, removed) in [
        ("use self::Tr::*;", "Tr", "f"),
        ("use self::Tr::{f, C};", "Tr", "f"),
        ("use self::E::*;", "E", "B"),
    ] {
        let decls = |with: bool| {
            let f = if with { "    fn f();\n" } else { "" };
            let b = if with { " B," } else { "" };
            format!("{FEATURE}trait Tr {{\n{f}    const C: u8;\n}}\nenum E {{ A,{b} }}\n{import}\n")
        };
        let mut loaded = Loaded::new(&format!("//- /main.rs crate:main\n{}", decls(true)));
        let main = loaded.krate();
        let has = |db: &Db| {
            let root = module_at(db, main, &[]);
            let map = crate_local_def_map(db, main).def_map(db);
            map.module(db, root).scope(db).get(&name(removed)).take_values().is_some()
        };
        assert!(has(&loaded.db), "{import}: loaded: `{removed}` of `{item}` is not imported");
        let file = loaded.file("/main.rs");
        loaded.db.set_file_text(file, &decls(false));
        assert!(!has(&loaded.db), "{import}: `{removed}` removed from `{item}`: it is still imported");
        check_db(&loaded.db, import, &Exempt::NONE, true, &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

/// A proc macro's crate resolved before its macros were built names no expander for them; given
/// them later, as `Lowered::macros` gives a built dylib's, it resolves again, and so does the
/// derive of a crate depending on it.
#[test]
fn a_proc_macro_crate_given_its_macros_after_its_map_was_read_resolves_them_again() {
    use either::Either;
    use hir_expand::{MacroDefKind, proc_macro::ProcMacros};

    use crate::MacroId;

    let mut loaded = Loaded::new(
        "//- proc_macros: derive_identity\n//- minicore: derive\n//- /main.rs\n#[derive(proc_macros::DeriveIdentity)]\npub struct S;\n",
    );
    let main = loaded.file("/main.rs");
    let db = &loaded.db;
    let main = *all_crates(db).iter().find(|it| it.data(db).root_file_id == main).expect("`main`");
    let macros = *all_crates(db).iter().find(|it| it.data(db).target_kind.is_proc_macro()).expect("`proc_macros`");
    let built = ProcMacros::get_for_crate(db, macros).and_then(|it| it.macros().map(<[_]>::to_vec)).expect("listed");
    // The expander of `DeriveIdentity` in `proc_macros`' map, and of the call of it in `main`'s.
    let expanders = |db: &Db| {
        let map = crate_local_def_map(db, macros).def_map(db);
        let defined = match map.module(db, map.root).scope(db).get(&name("DeriveIdentity")).take_macros() {
            Some(MacroId::ProcMacroId(it)) => !it.loc(db).expander.is_missing(),
            other => panic!("`DeriveIdentity` is not a proc macro of `proc_macros`: {other:?}"),
        };
        let map = crate_local_def_map(db, main).def_map(db);
        let calls: Vec<bool> = map
            .module(db, map.root)
            .scope(db)
            .derive_macro_invocs()
            .flat_map(|(_, invocs)| invocs.flat_map(|(_, _, calls)| calls.iter().filter_map(|it| it.and_then(Either::left))))
            .map(|call| match call.loc(db).def.kind {
                MacroDefKind::ProcMacro(_, expander, _) => !expander.is_missing(),
                other => panic!("the derive of `S` calls no proc macro: {other:?}"),
            })
            .collect();
        (defined, calls)
    };
    assert_eq!(expanders(&loaded.db), (true, vec![true]), "loaded with its macros built");

    ProcMacros::give(&mut loaded.db, [(macros, Err(base_db::ProcMacroLoadingError::NotYetBuilt))]);
    assert_eq!(expanders(&loaded.db), (false, vec![false]), "its macros not yet built");

    ProcMacros::give(&mut loaded.db, [(macros, Ok(built))]);
    assert_eq!(expanders(&loaded.db), (true, vec![true]), "given its built macros after its map was read");
    let mut failures = String::new();
    check_db(&loaded.db, "given built macros", &Exempt::NONE, true, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

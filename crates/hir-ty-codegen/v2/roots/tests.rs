//! The roots of each kind of unit over fixtures.

#[path = "../tests/db.rs"]
mod db;

use blaze_codegen::{plan::Mode, settings::Panic};
use hir_def::{AssocItemId, test_items};
use hir_expand::fixture;

use super::{
    super::{
        BuildTime, Databases, HashSource, HirFrontend, HirInstance, HirStatic, Kind, by_id::ById,
    },
    RootKind, Roots,
};
use db::TestDB;


/// What a test of `harness` is given as usercode's fork server: its two names, no more.
const FORK_SERVER: &str = "\n    pub mod fork_server {\n        \
     pub const SOCKET: &str = \"__BLAZE_FORK_SERVER\";\n        \
     pub fn serve(_: &'static [&'static super::test::TestDescAndFn], _: ::std::ffi::OsString) -> ! {\n            \
     loop {}\n        \
     }\n    \
     }\n";

fn fork_server(_: bool) -> &'static str {
    FORK_SERVER
}

/// The fixture lives as long as the test process, as the frontend borrows it.
fn frontend(fixture: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(fixture).into_db()));
    let frontend =
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), std::sync::Arc::new(BuildTime(Default::default())));
    (db, frontend)
}

fn roots(fixture: &str, kind: RootKind) -> (&'static TestDB, HirFrontend<'static>, Roots) {
    let (db, frontend) = frontend(fixture);
    let roots = frontend.roots(fixture::test_crate(&db), kind).expect("the roots");
    (db, frontend, roots)
}

fn refusal(fixture: &str, kind: RootKind) -> String {
    let (db, frontend) = frontend(fixture);
    frontend.roots(fixture::test_crate(&db), kind).expect_err("a refusal").what
}

#[track_caller]
fn function(db: &TestDB, frontend: &HirFrontend<'_>, name: &str) -> HirInstance {
    frontend.root(test_items::function(db, fixture::test_crate(db), name))
}

/// `fixture`, a crate with `cfg(test)`, with the harness `roots` asks for at the end of its
/// root file.
fn with_harness(fixture: &str) -> String {
    let (db, frontend) = frontend(fixture);
    let harness = frontend.harness(fixture::test_crate(&db), Panic::Unwind, Mode::Output, fork_server);
    let harness = harness.expect("a harness").expect("the crate has `cfg(test)`");
    format!("{fixture}{harness}")
}

/// The C `main` of `main`, the program's own kept symbols, nothing else of its crate.
#[test]
fn an_executable_starts_at_main_and_keeps_its_exports() {
    let (db, frontend, roots) = roots(
        r#"
//- /main.rs crate:main
fn main() {}
fn unused() {}
#[unsafe(no_mangle)]
pub extern "C" fn exported() {}
#[used]
static KEPT: u8 = 0;
"#,
        RootKind::Executable,
    );
    let main = test_items::function(db, fixture::test_crate(&db), "main");
    assert_eq!(roots.roots[0], HirInstance(Kind::Start(main)));
    assert!(roots.roots.contains(&function(db, &frontend, "exported")));
    assert!(!roots.roots.contains(&function(db, &frontend, "unused")));
    let kept = test_items::static_item(db, fixture::test_crate(&db), "KEPT");
    assert_eq!(roots.statics, [HirStatic(kept)]);
    assert!(roots.tests.is_empty());
}

/// A `#[linkage]` other than `internal` keeps a function or static, as rustc's
/// `contains_extern_indicator` does.
#[test]
fn an_executable_keeps_what_has_a_linkage_of_its_own() {
    let (db, frontend, roots) = roots(
        r#"
//- /main.rs crate:main
#![feature(linkage)]
fn main() {}
#[linkage = "weak"]
pub extern "C" fn weak() {}
#[linkage = "internal"]
fn internal() {}
#[linkage = "external"]
static LINKED: u8 = 0;
#[linkage = "internal"]
static HIDDEN: u8 = 0;
"#,
        RootKind::Executable,
    );
    assert!(roots.roots.contains(&function(db, &frontend, "weak")));
    assert!(!roots.roots.contains(&function(db, &frontend, "internal")));
    let linked = test_items::static_item(db, fixture::test_crate(&db), "LINKED");
    assert_eq!(roots.statics, [HirStatic(linked)]);
}

/// A dependency's `#[used]` static is in the program only when the program's code names the
/// dependency, as rustc loads only those, though cargo hands it every one.
#[test]
fn an_executable_keeps_what_only_the_dependencies_it_names_keep() {
    let (db, _, roots) = roots(
        r#"
//- /main.rs crate:main deps:named,unnamed
use named as _;
fn main() {}
//- /named.rs crate:named
#[used]
static NAMED: u8 = 0;
//- /unnamed.rs crate:unnamed
#[used]
static UNNAMED: u8 = 0;
"#,
        RootKind::Executable,
    );
    let main = fixture::test_crate(db);
    let dep = |name: &str| {
        let deps = &main.data(db).dependencies;
        deps.iter().find(|it| it.name.as_str() == name).expect("a dependency of the fixture").crate_id
    };
    let named = test_items::static_item(db, dep("named"), "NAMED");
    assert_eq!(roots.statics, [HirStatic(named)]);
    assert_eq!(&**hir_def::nameres::named::named_dependencies(db, main), [dep("named")]);
}

#[test]
fn no_main_has_no_start() {
    let (db, frontend, roots) = roots(
        r#"
//- /main.rs crate:main
#![no_main]
#[unsafe(no_mangle)]
pub extern "C" fn main(_: i32, _: *const *const u8) -> i32 { 0 }
"#,
        RootKind::Executable,
    );
    assert!(!roots.roots.iter().any(|it| matches!(it.0, Kind::Start(_))));
    assert!(roots.roots.contains(&function(db, &frontend, "main")));
}

#[test]
fn a_program_without_main_is_refused_as_rustc_refuses_it() {
    let what = refusal("//- /main.rs crate:main\nfn other() {}\n", RootKind::Executable);
    assert!(what.contains("E0601"), "{what}");
    let what = refusal("//- /main.rs crate:main\nfn main<T>() {}\n", RootKind::Executable);
    assert!(what.contains("E0131"), "{what}");
}

/// A test target with `harness = false` has no `cfg(test)`: its `main` is its own.
#[test]
fn a_test_without_a_harness_is_an_executable() {
    let (db, _, roots) = roots("//- /main.rs crate:main\nfn main() {}\n", RootKind::Test);
    let main = test_items::function(db, fixture::test_crate(&db), "main");
    assert_eq!(roots.roots[0], HirInstance(Kind::Start(main)));
    assert!(roots.tests.is_empty());
}

/// What another crate names, not generic, and what the library keeps; nothing private.
#[test]
fn a_library_roots_what_others_name() {
    let (db, frontend, roots) = roots(
        r#"
//- /lib.rs crate:lib
pub fn named() {}
fn private() {}
pub fn generic<T>() {}
mod hidden { pub fn reexported() {} pub fn not_reexported() {} }
pub use hidden::reexported;
pub mod open { pub fn inner() {} }
pub struct S;
impl S { pub fn method(&self) {} fn private_method(&self) {} }
pub fn uses_inside() {
    #[used]
    static INSIDE: u8 = 1;
}
"#,
        RootKind::Library,
    );
    for name in ["named", "reexported", "inner", "uses_inside"] {
        assert!(roots.roots.contains(&function(db, &frontend, name)), "{name} is a root");
    }
    for name in ["private", "not_reexported"] {
        assert!(!roots.roots.contains(&function(db, &frontend, name)), "{name} is not a root");
    }
    let imp = test_items::impls(db, fixture::test_crate(&db))[0];
    let methods: Vec<HirInstance> = (imp.impl_items(db).items.iter())
        .filter_map(|&(_, it)| match it {
            AssocItemId::FunctionId(it) => Some(frontend.root(it)),
            _ => None,
        })
        .collect();
    let [method, private_method] = &methods[..] else { panic!("two methods") };
    assert!(roots.roots.contains(method));
    assert!(!roots.roots.contains(private_method));
    assert_eq!(roots.statics.len(), 1, "the `#[used]` static in a body is kept");
}

/// libtest's order is by name; each test's root is its function, after the harness's start.
#[test]
fn a_harness_lists_its_tests_by_name() {
    let fixture = with_harness(
        r#"
//- /lib.rs crate:main cfg:test
#[test]
#[ignore = "slow"]
fn b() {}
#[test]
#[should_panic(expected = "boom")]
fn a() {}
#[test]
#[cfg_attr(test, ignore)]
#[should_panic]
fn c() {}
mod m {
    #[test]
    fn inner() {}
}
fn helper() {}
"#,
    );
    let (db, frontend, roots) = roots(&fixture, RootKind::Test);
    let names: Vec<&str> = roots.tests.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "m::inner"]);
    assert!(matches!(roots.roots[0].0, Kind::Start(_)));
    for test in &roots.tests {
        let own = test.name.rsplit("::").next().expect("a name");
        assert_eq!(roots.roots[test.root], function(db, &frontend, own), "{}", test.name);
    }
    let [a, b, c, _] = &roots.tests[..] else { panic!("four tests") };
    assert_eq!((a.ignore, &a.should_panic), (false, &Some(Some("boom".to_owned()))));
    assert_eq!((b.ignore, &b.should_panic), (true, &None));
    assert_eq!((c.ignore, &c.should_panic), (true, &Some(None)));
    assert!(roots.tests.iter().all(|it| !it.bench && !it.outside));
    assert!(!roots.roots.contains(&function(db, &frontend, "helper")));
}

#[test]
fn a_harness_written_for_other_tests_is_refused() {
    let fixture = with_harness("//- /lib.rs crate:main cfg:test\n#[test]\nfn a() {}\n");
    let fixture = fixture.replacen("fn a() {}\n", "fn a() {}\n#[test]\nfn added() {}\n", 1);
    let what = refusal(&fixture, RootKind::Test);
    assert!(what.contains("written for other tests"), "{what}");
}

#[test]
fn a_crate_with_tests_and_no_harness_is_refused() {
    let what = refusal("//- /lib.rs crate:main cfg:test\n#[test]\nfn a() {}\n", RootKind::Test);
    assert!(what.contains("no test harness"), "{what}");
}

#[test]
fn a_harness_with_panic_abort_is_refused() {
    let (db, frontend) = frontend("//- /lib.rs crate:main cfg:test\n#[test]\nfn a() {}\n");
    let what = frontend.harness(fixture::test_crate(&db), Panic::Abort, Mode::Output, fork_server).expect_err("refused");
    assert!(what.what.contains("-Zpanic-abort-tests"), "{}", what.what);
}

/// A test that starts a process or opens a socket always runs.
#[test]
fn a_test_reaching_the_network_is_outside() {
    let fixture = with_harness(
        r#"
//- /lib.rs crate:main cfg:test
unsafe extern "C" {
    fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
}
fn connects() { unsafe { socket(0, 0, 0); } }
#[test]
fn net() { connects() }
#[test]
fn pure() {}
"#,
    );
    let (_, _, roots) = roots(&fixture, RootKind::Test);
    let outside: Vec<(&str, bool)> =
        roots.tests.iter().map(|it| (it.name.as_str(), it.outside)).collect();
    assert_eq!(outside, [("net", true), ("pure", false)]);
}

#[test]
fn a_proc_macro_crate_needs_to_be_one() {
    let what = refusal("//- /lib.rs crate:main\npub fn f() {}\n", RootKind::ProcMacro);
    assert!(what.contains("not a proc-macro crate"), "{what}");
}

#[test]
fn the_text_on_disk_is_before_what_blaze_appends() {
    let fixture = "//- /lib.rs crate:main cfg:test\n#[test]\nfn a() {}\n";
    let (db, frontend) = frontend(fixture);
    let harness = frontend.harness(fixture::test_crate(&db), Panic::Unwind, Mode::Output, fork_server);
    let harness = harness.expect("a harness").expect("cfg(test)");
    let text = format!("fn a() {{}}\n{harness}");
    assert_eq!(HirFrontend::on_disk(&text), "fn a() {}\n");
    assert_eq!(HirFrontend::on_disk("fn a() {}\n"), "fn a() {}\n");
}

#[test]
fn a_crate_without_cfg_test_has_no_harness() {
    let (db, frontend) = frontend("//- /main.rs crate:main\nfn main() {}\n");
    let harness = frontend.harness(fixture::test_crate(&db), Panic::Unwind, Mode::Output, fork_server);
    assert_eq!(harness.expect("no refusal"), None);
}

/// A binary with jump-table slots asks for the fork server that records what each test ran.
#[test]
fn a_harness_records_where_its_binary_has_slots() {
    let (db, frontend) = frontend("//- /lib.rs crate:main cfg:test\n#[test]\nfn a() {}\n");
    let asked = |record: bool| -> &'static str {
        match record {
            true => "\n    // recorded\n",
            false => "\n    // unrecorded\n",
        }
    };
    for (mode, record) in [(Mode::Output, false), (Mode::Live, true), (Mode::Tiered, true)] {
        let harness = frontend.harness(fixture::test_crate(&db), Panic::Unwind, mode, asked);
        let harness = harness.expect("a harness").expect("cfg(test)");
        let wanted = if record { "// recorded" } else { "// unrecorded" };
        assert!(harness.contains(wanted), "{mode:?}: {harness}");
    }
}

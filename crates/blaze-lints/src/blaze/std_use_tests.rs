//! `std_use` over fixtures of its own: a `std` and an `alloc` with what the table's rows name,
//! a `bstd` that re-exports `Arc` as `Shared`, and a crate that uses them through full paths,
//! `use`s, aliases and re-exports. The crate linted is the one at `/src/lib.rs`.

use base_db::FileId;
use blaze_diagnostics::lints::registry::ready;
use hir_expand::fixture;

use super::Lint;
use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::{self, CommandLine, Level, Levels},
    tests::{CLIPPY_OFF, lint},
};

/// `alloc`, `std` and `bstd`, as far as the rows these tests use: where each item is defined
/// is where Rust defines it (`Arc` and `Vec` in `alloc`, re-exported by `std`), since a row is
/// matched by the crate and module of the definition, never by the path written.
const CRATES: &str = r#"
//- /alloc/lib.rs crate:alloc
pub mod vec {
    pub struct Vec<T>(T);
    impl<T> Vec<T> {
        pub fn new() -> Vec<T> {
            loop {}
        }
        pub fn len(&self) -> usize {
            loop {}
        }
    }
}
pub mod sync {
    pub struct Arc<T>(T);
    impl<T> Arc<T> {
        pub fn new(_value: T) -> Arc<T> {
            loop {}
        }
    }
}
//- /std/lib.rs crate:std deps:alloc
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub use alloc::vec;
pub mod sync {
    pub use alloc::sync::Arc;
}
pub mod fs {
    pub struct File(u32);
    impl File {
        pub fn open(_path: &str) -> File {
            loop {}
        }
        pub fn sync_all(&self) -> u32 {
            loop {}
        }
    }
    pub fn read_to_string(_path: &str) -> u32 {
        loop {}
    }
}
pub mod io {
    pub trait Write {
        fn write_all(&mut self, _text: &str) -> u32 {
            loop {}
        }
    }
    impl Write for crate::fs::File {}
    pub fn _print(_line: &str) {}
}
pub mod env {
    pub fn split_paths(_paths: &str) -> u32 {
        loop {}
    }
}
pub mod time {
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant {
            loop {}
        }
    }
}
#[macro_export]
macro_rules! println {
    ($line:literal) => {
        $crate::io::_print($line)
    };
}
#[macro_export]
macro_rules! thread_local {
    ($($tokens:tt)*) => {};
}
//- /bstd/lib.rs crate:bstd deps:alloc
pub mod sync {
    pub use alloc::sync::Arc as Shared;
}
"#;

/// A crate on std: the file system by a full path and through an alias of the module, a type
/// through a `use`, a trait's method on its value, a call that hides its cost, the clock, a
/// macro, a global-heap allocation and a method on its value, `core` computation, a function of
/// `env` that only computes; then `Arc` by std's name twice and by bstd's once. The first line
/// is line 1 of `src/lib.rs`.
const MAIN: &str = r#"use std::fs as files;
use std::fs::File;
use std::io::Write as _;
use std::sync::Arc;
pub fn load() -> u32 {
    let _a = std::fs::read_to_string("a");
    let _b = files::read_to_string("b");
    let mut file = File::open("c");
    let _n = file.write_all("x");
    let _s = file.sync_all();
    let _now = std::time::Instant::now();
    std::println!("loaded");
    let v = std::vec::Vec::<u32>::new();
    let _len = v.len();
    let _some = Some(1u32).is_some();
    std::env::split_paths("a:b")
}
pub fn share() -> u32 {
    let _a = Arc::new(1u32);
    let _b = std::sync::Arc::new(2u32);
    let _c = bstd::sync::Shared::new(3u32);
    std::fs::read_to_string("d")
}
"#;

fn fixture_of(krate: &str, main: &str) -> String {
    format!(
        "//- minicore: sized, option\n{}//- /src/lib.rs {krate} deps:std,bstd\n{main}",
        CRATES.trim_start_matches('\n')
    )
}

/// Every lint off but those `flags` turn on, so a fixture's findings are these lints' alone.
fn flags(flags: &str) -> String {
    format!("{CLIPPY_OFF} -A blaze::general {flags}")
}

/// The findings of `std_use` in `src/lib.rs` of `out`, as (line, message), in the order of the
/// lines.
fn found(out: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let mut message = None;
    for line in out.lines() {
        if let Some(it) = line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: ")) {
            let ours = it.starts_with('`') && (it.contains("), used ") || it.contains("), declared here, "));
            message = ours.then(|| it.to_owned());
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        let Some(message) = message.take() else { continue };
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push((number.expect("a line number after the path"), message));
    }
    found.sort();
    found
}

/// What `found` has of [`MAIN`], but its global-heap allocation: each line and how its message
/// starts.
fn main_findings() -> Vec<(u32, &'static str)> {
    vec![
        (6, "`std::fs::read_to_string` (ambient authority), used 2 times in `fn load`, takes the file system from nowhere: take a `Dir`"),
        (8, "`std::fs::File` (ambient authority), used 2 times in `fn load`, takes the file system from nowhere: take a `Dir` and open beneath it"),
        (10, "`std::fs::File::sync_all` (hidden cost), used once in `fn load`, waits for the device with no deadline"),
        (11, "`std::time::Instant::now` (ambient authority), used once in `fn load`, reads the clock from nowhere: take a `Clock` and call `clock.now()`"),
        (12, "`std::println!` (ambient authority), used once in `fn load`, writes to the process's standard streams from nowhere"),
        (19, "`std::sync::Arc` (renamed), used 2 times in `fn share`, has one name in bstd, and this is not it: write `bstd::sync::Shared`"),
        (22, "`std::fs::read_to_string` (ambient authority), used once in `fn share`, takes the file system from nowhere"),
    ]
}

const VEC_NEW: (u32, &str) =
    (13, "`std::vec::Vec::new` (global-heap allocation), used once in `fn load`, allocates from the global heap");

fn assert_found(out: &str, mut expected: Vec<(u32, &str)>) {
    expected.sort();
    let found = found(out);
    let lines: Vec<u32> = found.iter().map(|it| it.0).collect();
    let wanted: Vec<u32> = expected.iter().map(|it| it.0).collect();
    assert_eq!(lines, wanted, "the lines with a finding:\n{out}");
    for ((line, message), (_, starts)) in found.iter().zip(&expected) {
        assert!(message.starts_with(starts), "line {line}: `{message}` starts `{starts}`\n{out}");
    }
}

/// One finding per item per function, at its first use, with its count there: a path through a
/// `use`, one through an alias of the module and a full path are the same item; a trait's
/// method on a `File` is a use of `File`; `sync_all` is its own row; a macro is found by what it
/// expands to; each function has its own finding of an item both use. `core` computation, a
/// function that only computes and a method on a `Vec` are none.
#[test]
fn each_item_is_found_once_per_function_with_its_count() {
    let out = lint(&fixture_of("crate:main", MAIN), &flags("--warn=blaze::std_use"));
    let mut expected = main_findings();
    expected.push(VEC_NEW);
    assert_found(&out, expected);
    assert!(out.contains("warning: `std::fs::read_to_string`"), "at `warn`:\n{out}");
    for never in ["split_paths", "is_some", "Vec::len", "`std::vec::Vec`", "_print", "write_all"] {
        assert!(!found(&out).iter().any(|it| it.1.contains(never)), "{never} is no finding:\n{out}");
    }
    // What each kind says, the help and where the plan is.
    assert!(out.contains("with no capability, code is pure computation"), "{out}");
    assert!(out.contains("the call decides something costly that none of its arguments says"), "{out}");
    assert!(out.contains("bstd has no global heap"), "{out}");
    let findings = found(&out).len();
    assert_eq!(out.matches(Lint::StdUse.help()).count(), findings, "{out}");
    assert!(out.contains("lints.md#std_use"), "{out}");
    assert!(Lint::StdUse.help().contains("docs/v2/plan/design/std.md"));
    assert_eq!(
        out.contains("is planned and not in this blaze"),
        !ready::BSTD_IN_TREE,
        "a finding says whether bstd can be written against today:\n{out}"
    );
    assert_eq!(Lint::StdUse.help().contains("nothing to change yet"), !ready::BSTD_IN_TREE);
}

/// A renamed type is one definition under two names: the path written by std's name is the
/// use, and the same function reached by bstd's name is not, or the port could never end.
#[test]
fn a_renamed_type_counts_only_by_stds_name() {
    let ported = MAIN
        .replace("let _a = Arc::new(1u32);", "let _a = bstd::sync::Shared::new(1u32);")
        .replace("let _b = std::sync::Arc::new(2u32);", "let _b = bstd::sync::Shared::new(2u32);");
    let out = lint(&fixture_of("crate:main", &ported), &flags("--warn=blaze::std_use"));
    assert!(!found(&out).iter().any(|it| it.1.contains("(renamed)")), "{out}");
    assert!(found(&out).iter().any(|it| it.0 == 22), "the rest is found as before:\n{out}");
}

/// A row another lint already reports is left to it wherever that lint is on: with
/// `hot_alloc` on, `Vec::new` has no finding of this lint, and every other row is as before.
#[test]
fn a_row_hot_alloc_covers_is_not_reported_twice() {
    let fixture = fixture_of("crate:main", MAIN);
    for covering in ["blaze::hot_alloc", "blaze::implicit_allocator"] {
        let out = lint(&fixture, &flags(&format!("--warn=blaze::std_use --warn={covering}")));
        assert_found(&out, main_findings());
        assert!(!out.contains("(global-heap allocation)"), "{covering} has the row:\n{out}");
    }
}

/// `allow` by default, whatever the switch says: a crate that did not ask has no finding.
#[test]
fn a_crate_that_did_not_ask_has_no_finding() {
    let out = lint(&fixture_of("crate:main", MAIN), &flags(""));
    assert!(found(&out).is_empty(), "{out}");
    let levels = Levels::new(&CommandLine::default());
    assert_eq!(levels.of(level::Lint::Blaze(Lint::StdUse), &[]).level, Level::Allow);
}

/// `#![blaze::capabilities]` at the crate's root asks for it at `warn`; an `allow` after it
/// wins, as between any two lint attributes.
#[test]
fn a_crate_with_the_capabilities_attribute_has_them_at_warn() {
    let asked = format!("#![blaze::capabilities]\n{MAIN}");
    let out = lint(&fixture_of("crate:main", &asked), &flags(""));
    let mut expected = main_findings();
    expected.push(VEC_NEW);
    let moved: Vec<(u32, &str)> = expected.into_iter().map(|(line, it)| (line + 1, it)).collect();
    assert_found(&out, moved);
    assert!(out.contains("warning: `std::fs::read_to_string`"), "at `warn`:\n{out}");
    assert!(!out.contains("error: `std::"), "{out}");
    let silenced = format!("#![blaze::capabilities]\n#![allow(blaze::std_use)]\n{MAIN}");
    let out = lint(&fixture_of("crate:main", &silenced), &flags(""));
    assert!(found(&out).is_empty(), "{out}");
}

/// What `std_use = "warn"` in a workspace's `lints-when-ready` table becomes once the switch is
/// on is `--warn=blaze::std_use` (blaze-units' `workspace_lints`, whose own test holds blaze's table
/// to the switch): under that flag a workspace crate's findings are warnings, and a
/// dependency's (`--cap-lints allow`) are none.
#[test]
fn the_workspaces_flag_has_them_at_warn_and_a_dependency_has_none() {
    let out = lint(&fixture_of("crate:main", MAIN), &flags("--warn=blaze::std_use"));
    assert!(!found(&out).is_empty(), "{out}");
    assert!(!out.contains("error: `std::"), "{out}");
    let capped = lint(
        &fixture_of("crate:main library", MAIN),
        &flags("--warn=blaze::std_use --cap-lints allow"),
    );
    assert!(found(&capped).is_empty(), "{capped}");
}

/// A build script is `build_script_ambient`'s, and test code drives the crate from outside:
/// neither has a finding of this lint.
#[test]
fn a_build_script_and_test_code_have_no_finding() {
    let out =
        lint(&fixture_of("crate:custom-build:main", MAIN), &flags("--warn=blaze::std_use"));
    assert!(found(&out).is_empty(), "{out}");
    let tested = "#[cfg(test)]\nmod tests {\n    fn read() -> u32 {\n        \
                  std::fs::read_to_string(\"a\")\n    }\n}\n";
    let out = lint(
        &fixture_of("crate:main cfg:test", tested),
        &flags("--warn=blaze::std_use -A dead_code"),
    );
    assert!(found(&out).is_empty(), "{out}");
}

/// A `static mut` and a `thread_local!` are found where they are declared, the macro by the
/// crate that defines it: one of the same name the crate defines itself is none.
#[test]
fn global_state_is_found_where_it_is_declared() {
    let main = "pub static mut COUNT: u32 = 0;\n\
                pub static FIXED: u32 = 0;\n\
                std::thread_local! { static SLOT: u32 = 0; }\n\
                macro_rules! thread_local { ($($tokens:tt)*) => {}; }\n\
                thread_local! { static OWN: u32 = 0; }\n";
    let out = lint(&fixture_of("crate:main", main), &flags("--warn=blaze::std_use"));
    assert_found(
        &out,
        vec![
            (1, "`static mut` (ambient authority), declared here, is global mutable state"),
            (3, "`std::thread_local!` (ambient authority), declared here, keeps state per thread"),
        ],
    );
}

/// Linting the same crate twice writes the same bytes (`deterministic-output`).
#[test]
fn the_findings_are_the_same_each_run() {
    let fixture = fixture_of("crate:main", MAIN);
    let flags = flags("--warn=blaze::std_use");
    assert_eq!(lint(&fixture, &flags), lint(&fixture, &flags));
}

const FIXABLE: &str = r#"
use std::sync::Arc;
pub fn share() -> u32 {
    let _a = Arc::new(1u32);
    let _b = std::sync::Arc::<u32>::new(2u32);
    0
}
"#;

/// The fix of a renamed type writes bstd's path where std's is written, keeping the type's
/// arguments, and leaves the file with nothing for the row; the fixed text is compared whole.
#[test]
fn the_fix_of_a_rename_writes_bstds_path() {
    let want = r#"
use std::sync::Arc;
pub fn share() -> u32 {
    let _a = bstd::sync::Shared::new(1u32);
    let _b = bstd::sync::Shared::<u32>::new(2u32);
    0
}
"#;
    let fx = fixture::db(&fixture_of("crate:main", FIXABLE.trim_start_matches('\n')));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || crate::tests::main_crate(&db));
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> =
        flags("--warn=blaze::std_use").split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let text = db.file_text(file).text(&db).to_string();
    assert_eq!(text, want.trim_start_matches('\n'));
    blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found
            .iter()
            .map(|it| it.diagnostic.message.as_str())
            .filter(|it| it.contains("(renamed)"))
            .collect();
        assert!(left.is_empty(), "the fixed file has nothing left for the row:\n{text}\n{left:#?}");
    });
}

/// A fix never writes a path that does not resolve: in a crate with no dependency named
/// `bstd` the finding is there and its fix is not.
#[test]
fn a_crate_that_cannot_name_bstd_is_offered_no_fix() {
    let with = lint(
        &fixture_of("crate:main", FIXABLE.trim_start_matches('\n')),
        &flags("--warn=blaze::std_use"),
    );
    assert!(with.contains("write it by bstd's name"), "{with}");
    let without = fixture_of("crate:main", FIXABLE.trim_start_matches('\n'))
        .replace("crate:main deps:std,bstd", "crate:main deps:std");
    let out = lint(&without, &flags("--warn=blaze::std_use"));
    assert_eq!(found(&out).len(), 1, "{out}");
    assert!(!out.contains("write it by bstd's name"), "{out}");
}

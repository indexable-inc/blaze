//! The rule pack `tools/astlog/rules-sema/`, each rule run over its fixture crate through a real
//! front end. The rule file and the fixture are read from the pack, not copied, so a rule that
//! stops loading or a fixture that stops matching fails here.
//!
//! A fixture is `tests/<rule>/`: every `src/lib.rs` under it with a `Cargo.toml` beside is a
//! crate (the root one depends on the others), and `expected.txt` holds the findings as
//! `blaze check --query` prints them, or `fixed.rs` the root crate's `src/lib.rs` after the
//! rule's rewrites. The view holds the fixture's files at their paths in the fixture and the
//! pack's `lib/` at its path in blaze's checkout, so an `(include ...)` resolves as the daemon
//! resolves it in that checkout: from the view's root.
//!
//! `std` is [`STD`], the few items the fixtures name, with the paths the real one has.

use std::path::{Path, PathBuf};

use blaze_api::query::{Cell, Rows, Severity};

use super::rewrite::after;
use super::{edit, query, view};

/// The pack, from this crate's directory.
const PACK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/astlog/rules-sema");

/// The pack's path in blaze's checkout, the view root an include is resolved from.
const PACK_IN_CHECKOUT: &str = "tools/astlog/rules-sema";

const STD: &str = r#"
//- /std/lib.rs crate:std edition:2024
pub mod string {
    pub struct String(u8);
    impl String {
        pub fn new() -> String { String(0) }
        pub fn from(_: &str) -> String { String(0) }
        pub fn len(&self) -> usize { 0 }
        pub fn push_str(&mut self, _: &str) {}
    }
    pub trait ToString { fn to_string(&self) -> String; }
    impl<T: ?Sized> ToString for T { fn to_string(&self) -> String { String(0) } }
}
pub mod borrow {
    pub enum Cow<'a, B: ?Sized + 'a> { Borrowed(&'a B), Owned(crate::string::String) }
    impl<'a> Cow<'a, str> {
        pub fn into_owned(self) -> crate::string::String { crate::string::String(0) }
    }
}
pub mod vec { pub struct Vec<T>(T); impl<T> Vec<T> { pub fn new() -> Vec<T> { loop {} } pub fn len(&self) -> usize { 0 } } }
pub mod option { pub enum Option<T> { None, Some(T) } }
pub mod result { pub enum Result<T, E> { Ok(T), Err(E) } }
pub mod ffi {
    pub struct OsStr(u8);
    impl OsStr {
        pub fn new(_: &str) -> &OsStr { loop {} }
        pub fn to_string_lossy(&self) -> crate::borrow::Cow<'_, str> { loop {} }
    }
    pub struct OsString(u8);
}
pub mod path {
    pub struct Path(u8);
    impl Path {
        pub fn new(_: &str) -> &Path { loop {} }
        pub fn join(&self, _: &str) -> PathBuf { loop {} }
        pub fn to_string_lossy(&self) -> crate::borrow::Cow<'_, str> { loop {} }
        pub fn as_os_str(&self) -> &crate::ffi::OsStr { loop {} }
        pub fn to_path_buf(&self) -> PathBuf { loop {} }
    }
    pub struct PathBuf(u8);
    impl PathBuf {
        pub fn from(_: &str) -> PathBuf { loop {} }
        pub fn join(&self, _: &str) -> PathBuf { loop {} }
        pub fn push(&mut self, _: &str) {}
        pub fn as_path(&self) -> &Path { loop {} }
    }
}
pub mod io {
    pub struct Error(u8);
    pub type Result<T> = crate::result::Result<T, Error>;
    pub struct Stdout(u8);
    pub struct Stderr(u8);
    pub fn stdout() -> Stdout { Stdout(0) }
    pub fn stderr() -> Stderr { Stderr(0) }
    pub trait Write { fn write_str(&mut self, _: &str) -> Result<()>; }
    impl Write for Stdout { fn write_str(&mut self, _: &str) -> Result<()> { loop {} } }
    impl Write for Stderr { fn write_str(&mut self, _: &str) -> Result<()> { loop {} } }
}
pub mod env {
    pub fn current_exe() -> crate::io::Result<crate::path::PathBuf> { loop {} }
    pub fn var(_: &str) -> crate::result::Result<crate::string::String, crate::io::Error> { loop {} }
    pub fn var_os(_: &str) -> crate::option::Option<crate::ffi::OsString> { loop {} }
}
pub mod process {
    pub struct Command(u8);
    pub struct ExitStatus(u8);
    impl ExitStatus { pub fn success(&self) -> bool { true } }
    impl Command {
        pub fn new<S>(_: S) -> Command { Command(0) }
        pub fn arg<S>(&mut self, _: S) -> &mut Command { self }
        pub fn status(&mut self) -> crate::io::Result<ExitStatus> { loop {} }
    }
}
pub mod thread {
    pub struct JoinHandle<T>(T);
    impl<T> JoinHandle<T> { pub fn join(self) -> crate::result::Result<T, ()> { loop {} } }
    pub fn spawn<F, T>(_: F) -> JoinHandle<T> { loop {} }
}
#[macro_export]
macro_rules! format { ($($t:tt)*) => { $crate::string::String::new() }; }
#[macro_export]
macro_rules! writeln { ($out:expr, $($t:tt)*) => { $crate::io::Write::write_str(&mut $out, "") }; }
#[macro_export]
macro_rules! assert { ($($t:tt)*) => { () }; }
#[macro_export]
macro_rules! assert_eq { ($($t:tt)*) => { () }; }
pub mod prelude {
    pub mod rust_2024 {
        pub use crate::option::Option::{self, None, Some};
        pub use crate::result::Result::{self, Err, Ok};
        pub use crate::string::{String, ToString};
        pub use crate::vec::Vec;
    }
}
"#;

/// A fixture crate: its directory under the fixture, relative (`""` for the root), and the
/// name its `Cargo.toml` gives it, as Rust spells it.
struct Krate {
    dir: String,
    name: String,
}

/// Each crate of the fixture at `root`, the root first.
fn crates(root: &Path) -> Vec<Krate> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(dir) = stack.pop() {
        let at = root.join(&dir);
        let manifest = at.join("Cargo.toml");
        if manifest.is_file() && at.join("src/lib.rs").is_file() {
            let text = std::fs::read_to_string(&manifest).unwrap();
            let name = text
                .lines()
                .find_map(|it| it.strip_prefix("name = \""))
                .and_then(|it| it.strip_suffix('"'))
                .unwrap_or_else(|| panic!("no package name in {}", manifest.display()));
            out.push(Krate { dir: dir.to_str().unwrap().to_owned(), name: name.replace('-', "_") });
        }
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&at)
            .unwrap()
            .map(|it| it.unwrap().path())
            .filter(|it| it.is_dir() && it.file_name().is_some_and(|it| it != "src"))
            .map(|it| it.strip_prefix(root).unwrap().to_owned())
            .collect();
        subdirs.sort();
        stack.extend(subdirs.into_iter().rev());
    }
    out.sort_by_key(|it| !it.dir.is_empty());
    out
}

/// The pack's shared rules files, each as its path in blaze's checkout and its text.
fn library() -> Vec<(String, String)> {
    let dir = Path::new(PACK).join("lib");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|it| it.unwrap().path())
        .map(|it| {
            let name = it.file_name().unwrap().to_str().unwrap().to_owned();
            (format!("{PACK_IN_CHECKOUT}/lib/{name}"), std::fs::read_to_string(&it).unwrap())
        })
        .collect();
    out.sort();
    out
}

/// The front end of `rule`'s fixture beside [`STD`] and the pack's library, and the rule's text.
fn fixture(rule: &str) -> (base_db::Db, super::View, String) {
    let root = Path::new(PACK).join("tests").join(rule);
    let crates = crates(&root);
    assert!(!crates.is_empty(), "no fixture crate under {}", root.display());
    let mut text = STD.to_owned();
    let mut paths: Vec<String> = Vec::new();
    // Dependencies first, so each crate's `deps:` names a crate already declared.
    for (at, krate) in crates.iter().enumerate().rev() {
        let path = match krate.dir.as_str() {
            "" => "src/lib.rs".to_owned(),
            dir => format!("{dir}/src/lib.rs"),
        };
        let mut deps = vec!["std".to_owned()];
        if at == 0 {
            deps.extend(crates[1..].iter().map(|it| it.name.clone()));
        }
        let source = std::fs::read_to_string(root.join(&path)).unwrap();
        text.push_str(&format!("//- /{path} crate:{} deps:{} edition:2024\n{source}", krate.name, deps.join(",")));
        paths.push(path);
    }
    for (path, rules) in library() {
        text.push_str(&format!("//- /{path}\n{rules}"));
        paths.push(path);
    }
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let (db, files) = view(&text, 1, &paths);
    let rules = std::fs::read_to_string(Path::new(PACK).join(format!("{rule}.astlog"))).unwrap();
    (db, files, rules)
}

/// The findings as `blaze check --query` prints them ahead of the relations.
fn shown(outcome: &Rows) -> String {
    let mut out = String::new();
    for finding in &outcome.findings {
        let severity = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let at = match &finding.at {
            Some(Cell::Code { path, line: Some(line), .. }) => format!("{path}:{line}"),
            Some(Cell::Expanded { call, .. }) => call.clone(),
            other => panic!("a finding at {other:?}"),
        };
        out.push_str(&format!("{severity}: {}\n  --> {at}\n", finding.message));
    }
    out
}

/// `rule`'s findings over its fixture against `expected.txt`.
fn lints(rule: &str) {
    let (db, files, rules) = fixture(rule);
    let outcome = query(&db, &files, &rules).unwrap_or_else(|error| panic!("{rule}: {error}"));
    let expected = Path::new(PACK).join("tests").join(rule).join("expected.txt");
    let want = std::fs::read_to_string(&expected).unwrap();
    let got = shown(&outcome);
    assert!(got == want, "{rule}: found\n{got}\nwhere {} has\n{want}", expected.display());
}

/// `rule`'s rewrites of its fixture's root crate against `fixed.rs`.
fn rewrites(rule: &str) {
    let (db, files, rules) = fixture(rule);
    let applied = edit(&db, &files, &rules, &[]).unwrap_or_else(|error| panic!("{rule}: {error}"));
    let texts = after(&files, &applied.edited.ops);
    let got = &texts.iter().find(|it| it.0 == "src/lib.rs").expect("the root crate").1;
    let fixed = Path::new(PACK).join("tests").join(rule).join("fixed.rs");
    let want = std::fs::read_to_string(&fixed).unwrap();
    assert!(*got == want, "{rule}: rewrote to\n{got}\nwhere {} has\n{want}", fixed.display());
}

#[test]
fn every_rule_of_the_pack_has_a_test_here() {
    let source = include_str!("rules_sema.rs");
    let mut rules: Vec<String> = std::fs::read_dir(PACK)
        .unwrap()
        .map(|it| it.unwrap().path())
        .filter(|it| it.extension().is_some_and(|it| it == "astlog"))
        .map(|it| it.file_stem().unwrap().to_str().unwrap().to_owned())
        .collect();
    rules.sort();
    assert_eq!(rules.len(), 12, "{rules:?}");
    for rule in rules {
        let call = [format!("lints(\"{rule}\")"), format!("rewrites(\"{rule}\")")];
        assert!(call.iter().any(|it| source.contains(it.as_str())), "no test runs `{rule}`");
        assert!(Path::new(PACK).join("tests").join(&rule).is_dir(), "`{rule}` has no fixture");
    }
}

#[test]
fn cargo_home_homes() {
    lints("cargo-home-homes");
}

#[test]
fn cow_to_string() {
    rewrites("cow-to-string");
}

#[test]
fn current_exe_respawn() {
    lints("current-exe-respawn");
}

#[test]
fn deprecated_forwarder_port() {
    rewrites("deprecated-forwarder-port");
}

#[test]
fn enum_cast_index() {
    lints("enum-cast-index");
}

#[test]
fn frame_current_outside_task() {
    lints("frame-current-outside-task");
}

#[test]
fn int_arm_variant_picker() {
    lints("int-arm-variant-picker");
}

#[test]
fn picker_count_const() {
    lints("picker-count-const");
}

#[test]
fn registry_misses_const() {
    lints("registry-misses-const");
}

#[test]
fn stdout_in_spawned_thread() {
    lints("stdout-in-spawned-thread");
}

#[test]
fn tag_collision() {
    lints("tag-collision");
}

#[test]
fn variant_table_disagrees() {
    lints("variant-table-disagrees");
}

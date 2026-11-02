//! The import worklist reaches the def map the loop over every import reaches, with the same
//! diagnostics in the same order, and retries fewer imports to get there.

use std::fmt::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_ids::rng::SplitMix64;

use super::*;
use crate::nameres::{
    collector::{RESOLVE_IMPORT_CALLS, RESOLVE_STRATEGY, ResolveStrategy},
    facts::tests::old_crate_local_def_map,
};

/// Every crate's def map with its diagnostics, collected by the old collector with `strategy`,
/// and the imports it resolved on the way; `None` if collecting panicked. The import loop is
/// the old collector's: the maintainer that resolves `crate_def_map` has none.
fn collected_with(
    strategy: ResolveStrategy,
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
) -> (Option<String>, u64) {
    let previous = RESOLVE_STRATEGY.replace(strategy);
    let calls = RESOLVE_IMPORT_CALLS.replace(0);
    let collected = catch_unwind(AssertUnwindSafe(|| {
        let db = fixture::db(ra_fixture).into_db();
        let mut out = String::new();
        for &krate in base_db::all_crates(&db).iter() {
            let def_map = old_crate_local_def_map(&db, krate).def_map(&db);
            out.push_str(&def_map.dump(&db));
            for diagnostic in def_map.diagnostics(&db) {
                writeln!(out, "{diagnostic:?}").unwrap();
            }
        }
        out
    }))
    .ok();
    let made = RESOLVE_IMPORT_CALLS.replace(calls);
    RESOLVE_STRATEGY.set(previous);
    (collected, made)
}

/// Collects `ra_fixture` both ways and asserts the def maps agree; the calls each made.
fn check_same(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> (u64, u64) {
    let (rounds, rounds_calls) = collected_with(ResolveStrategy::Rounds, ra_fixture);
    let (worklist, worklist_calls) = collected_with(ResolveStrategy::Worklist, ra_fixture);
    assert!(rounds == worklist, "worklist differs from rounds on\n{ra_fixture}");
    if rounds.is_some() {
        assert!(
            worklist_calls <= rounds_calls,
            "worklist made {worklist_calls} calls, rounds {rounds_calls}, on\n{ra_fixture}"
        );
    }
    (worklist_calls, rounds_calls)
}

/// The fixtures of a test file: its raw strings, but for the `expect!` ones.
fn fixtures(source: &str) -> impl Iterator<Item = &str> {
    let mut rest = source;
    std::iter::from_fn(move || {
        loop {
            let start = rest.find("r#\"")?;
            let before = rest[..start].trim_end();
            let body = &rest[start + 3..];
            let end = body.find("\"#")?;
            rest = &body[end + 2..];
            if !before.ends_with("[[") {
                return Some(&body[..end]);
            }
        }
    })
}

const TEST_SOURCES: [(&str, &str); 5] = [
    ("tests.rs", include_str!("../tests.rs")),
    ("tests/globs.rs", include_str!("globs.rs")),
    ("tests/imports.rs", include_str!("imports.rs")),
    ("tests/macros.rs", include_str!("macros.rs")),
    ("tests/mod_resolution.rs", include_str!("mod_resolution.rs")),
];

#[test]
fn every_nameres_fixture_is_the_same_with_the_worklist() {
    let mut seen = 0;
    for (file, source) in TEST_SOURCES {
        let mut in_file = 0;
        for fixture in fixtures(source) {
            check_same(fixture);
            in_file += 1;
        }
        assert!(in_file > 0, "no fixture found in {file}");
        seen += in_file;
    }
    assert!(seen > 100, "only {seen} fixtures found");
}

#[test]
fn fixtures_are_found_without_expectations() {
    let source = "check(\n    r#\"\nuse a::b;\n\"#,\n    expect![[r#\"\n        crate\n    \"#]],\n);";
    assert_eq!(fixtures(source).collect::<Vec<_>>(), ["\nuse a::b;\n"]);
}

const TYPES: [&str; 4] = ["A", "B", "C", "E"];
const FNS: [&str; 2] = ["f", "g"];
const MACROS: [&str; 2] = ["mk", "mm"];

/// One module of a generated crate.
struct Module {
    parent: Option<usize>,
    children: Vec<usize>,
}

/// A crate of `modules` modules, nested at random, each with items, `use`s (plain, renamed,
/// globs of modules and enums, re-exports, `super` cycles) and `macro_rules!` that define
/// items. Paths name modules of this crate and of `deps` and often do not resolve.
fn random_crate(rng: &mut SplitMix64, deps: &[String], out: &mut String) {
    let count = 1 + rng.index(6);
    let mut modules = vec![Module { parent: None, children: Vec::new() }];
    for at in 1..count {
        let parent = rng.index(at);
        modules.push(Module { parent: Some(parent), children: Vec::new() });
        modules[parent].children.push(at);
    }
    write_module(rng, deps, &modules, 0, 0, out);
}

fn write_module(
    rng: &mut SplitMix64,
    deps: &[String],
    modules: &[Module],
    at: usize,
    depth: usize,
    out: &mut String,
) {
    let pad = "    ".repeat(depth);
    let in_root = modules[at].parent.is_none();
    for _ in 0..rng.index(4) {
        let v = visibility(rng, in_root);
        let written = match rng.index(5) {
            0 => writeln!(out, "{pad}{v}struct {};", rng.pick(&TYPES).unwrap()),
            1 => writeln!(out, "{pad}{v}struct {} {{}}", rng.pick(&TYPES).unwrap()),
            2 => writeln!(out, "{pad}{v}enum {} {{ A, B(u8), C {{}} }}", rng.pick(&TYPES).unwrap()),
            3 => writeln!(out, "{pad}{v}fn {}() {{}}", rng.pick(&FNS).unwrap()),
            _ => writeln!(out, "{pad}{v}trait {} {{}}", rng.pick(&TYPES).unwrap()),
        };
        written.unwrap();
    }
    if rng.one_in(3) {
        let name = rng.pick(&MACROS).unwrap();
        if rng.one_in(2) {
            writeln!(out, "{pad}#[macro_export]").unwrap();
        }
        let item = rng.pick(&TYPES).unwrap();
        writeln!(out, "{pad}macro_rules! {name} {{ () => {{ pub struct {item}; }} }}").unwrap();
        writeln!(out, "{pad}{name}!();").unwrap();
    }
    if rng.one_in(4) {
        writeln!(out, "{pad}{}!();", rng.pick(&MACROS).unwrap()).unwrap();
    }
    for _ in 0..rng.index(5) {
        let v = visibility(rng, in_root);
        let path = random_path(rng, deps, modules, at);
        let alias = match rng.index(6) {
            0 => format!(" as {}", rng.pick(&TYPES).unwrap()),
            1 => " as _".to_owned(),
            _ => String::new(),
        };
        let alias = if path.ends_with('*') { "" } else { alias.as_str() };
        writeln!(out, "{pad}{v}use {path}{alias};").unwrap();
    }
    if !in_root && rng.one_in(4) {
        writeln!(out, "{pad}pub use super::*;").unwrap();
    }
    for &child in &modules[at].children {
        let v = if rng.one_in(4) { "" } else { "pub " };
        writeln!(out, "{pad}{v}mod m{child} {{").unwrap();
        write_module(rng, deps, modules, child, depth + 1, out);
        writeln!(out, "{pad}}}").unwrap();
    }
}

fn visibility(rng: &mut SplitMix64, in_root: bool) -> &'static str {
    match rng.index(if in_root { 3 } else { 5 }) {
        0 => "",
        1 => "pub ",
        2 => "pub(crate) ",
        3 => "pub(super) ",
        _ => "pub(self) ",
    }
}

/// A path from module `at`: a start (`crate`, `self`, `super`, a dependency, a plain name),
/// up to two module names, and an item, a variant or a glob.
fn random_path(rng: &mut SplitMix64, deps: &[String], modules: &[Module], at: usize) -> String {
    let mut path = match rng.index(5) {
        0 => "crate".to_owned(),
        1 => "self".to_owned(),
        2 if modules[at].parent.is_some() => "super".to_owned(),
        3 if !deps.is_empty() => deps[rng.index(deps.len())].clone(),
        _ => format!("m{}", rng.index(modules.len())),
    };
    for _ in 0..rng.index(3) {
        write!(path, "::m{}", rng.index(modules.len())).unwrap();
    }
    match rng.index(8) {
        0 | 1 => path.push_str("::*"),
        2 => write!(path, "::{}::*", rng.pick(&TYPES).unwrap()).unwrap(),
        3 => write!(path, "::{}::A", rng.pick(&TYPES).unwrap()).unwrap(),
        4 => write!(path, "::{}", rng.pick(&FNS).unwrap()).unwrap(),
        5 => write!(path, "::{}", rng.pick(&MACROS).unwrap()).unwrap(),
        _ => write!(path, "::{}", rng.pick(&TYPES).unwrap()).unwrap(),
    }
    path
}

/// A fixture of 1-4 crates, each depending on some of the ones before it.
fn random_graph(seed: u64) -> String {
    let mut rng = SplitMix64::new(seed);
    let mut out = String::new();
    let crates = 1 + rng.index(4);
    for at in 0..crates {
        let deps: Vec<String> = (0..at).filter(|_| rng.one_in(2)).map(|it| format!("c{it}")).collect();
        let edition = match rng.index(4) {
            0 => " edition:2015",
            1 => " edition:2018",
            _ => " edition:2021",
        };
        write!(out, "//- /c{at}/lib.rs crate:c{at}{edition}").unwrap();
        if !deps.is_empty() {
            write!(out, " deps:{}", deps.join(",")).unwrap();
        }
        out.push('\n');
        if !deps.is_empty() && rng.one_in(3) {
            writeln!(out, "#[macro_use] extern crate {};", deps[rng.index(deps.len())]).unwrap();
        }
        random_crate(&mut rng, &deps, &mut out);
    }
    out
}

#[test]
fn random_crate_graphs_are_the_same_with_the_worklist() {
    for seed in 0..300 {
        check_same(&random_graph(seed));
    }
}

#[test]
fn random_graphs_are_deterministic() {
    assert_eq!(random_graph(7), random_graph(7));
    assert_ne!(random_graph(7), random_graph(8));
}

/// `m0` imports `X` from `m1`, which imports it from `m2`, ..., declared in `m{n-1}`: each pass
/// of the loop over every import resolves one more, so it makes O(n²) calls; the worklist
/// retries an import only once the one it reads from is recorded, O(n).
fn import_chain(n: usize) -> String {
    let mut out = String::from("//- /lib.rs\n");
    for at in 0..n - 1 {
        writeln!(out, "pub mod m{at} {{ pub use super::m{}::X; }}", at + 1).unwrap();
    }
    writeln!(out, "pub mod m{} {{ pub struct X; }}", n - 1).unwrap();
    out
}

#[test]
fn a_long_import_chain_takes_the_worklist_linear_calls() {
    let n = 64;
    let (worklist, rounds) = check_same(&import_chain(n));
    let n = n as u64;
    assert!(rounds >= n * (n - 1) / 2, "rounds made {rounds} calls");
    assert!(worklist <= 3 * n, "worklist made {worklist} calls");
    assert!(worklist * 8 < rounds, "worklist {worklist}, rounds {rounds}");
}

#[test]
fn a_glob_cycle_reaches_the_same_fixed_point() {
    check_same(
        r#"
//- /lib.rs
pub mod a { pub use super::b::*; pub struct A; }
pub mod b { pub use super::c::*; pub struct B; }
pub mod c { pub use super::a::*; pub struct C; pub use super::d::D; }
pub mod d { pub use super::a::*; pub struct D; }
use a::{A, B, C, D};
"#,
    );
}

#[test]
fn an_import_shadowing_a_glob_reaches_the_same_fixed_point() {
    check_same(
        r#"
//- /lib.rs
mod x { pub struct T; pub mod inner { pub struct U; } }
mod y { pub mod inner { pub struct V; } }
use x::*;
use y::inner;
use inner::V;
pub use self::later::*;
mod later { pub use crate::y::inner as T; }
"#,
    );
}

#[test]
fn an_enum_reached_through_a_renamed_import_waits_on_its_declaration() {
    check_same(
        r#"
//- /lib.rs
mod m {
    pub(crate) enum E { A, B }
    pub use self::E as R;
}
use m::R::A;
use m::R::*;
"#,
    );
}

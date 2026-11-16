//! Items declared inside a body, through the real front end with the installed toolchain
//! (`$HOME/.blaze/v2/toolchain`, as tests/front2_checks.rs): each fixture is a fn whose BODY is
//! checked by front2 and must check clean, or be refused with the finding named.
//!
//! The Reference, "Statements", "Item declarations" (fetched 2026-11-16):
//! [statement.item.intro] "An item declaration statement has a syntactic form identical to an
//! item declaration within a module." [statement.item.scope] "Declaring an item within a
//! statement block restricts its scope to the block containing the statement. The item is not
//! given a canonical path nor are any sub-items it may declare."
//! [statement.item.associated-scope] "The exception to this is that associated items defined by
//! implementations are still accessible in outer scopes as long as the item and, if applicable,
//! trait are accessible. It is otherwise identical in meaning to declaring the item inside a
//! module." [statement.item.outer-generics] "There is no implicit capture of the containing
//! function's generic parameters, parameters, and local variables."
//! "Names", "Scopes", item scopes: an item declared in a block is in scope in the whole block,
//! before and after its declaration, and shadows the outer names of its namespace.
//!
//! The pinned core's `assert_unsafe_precondition!` (library/core/src/ub_checks.rs) expands, in
//! the calling body, to a block that declares `const fn precondition_check($($name:$ty),*) {..}`
//! and then calls `precondition_check($($arg,)*)`: `local_macro` is its shape, and
//! `core::slice::from_raw_parts` (which calls it) is checked by its path.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn local_fn() -> u8 { fn inner() -> u8 { 1 } inner() }
fn local_fn_before() -> u8 { let x = inner(); fn inner() -> u8 { 1 } x }
fn local_const() -> u8 { const K: u8 = 2; K }
fn local_struct() -> u8 { struct P(u8); impl P { fn get(&self) -> u8 { self.0 } } P(3).get() }
fn local_use() -> u8 { use core::cmp::max; max(1, 2) }
fn local_shadows() -> u8 { fn outer() -> u8 { 5 } outer() }
fn outer() -> bool { true }
fn out_of_block(x: u8) -> u8 { { fn inner() -> u8 { 4 } } inner() }
macro_rules! check_shape {
    ($e:expr) => {
        {
            const fn precondition_check(a: usize) {
                if !(a > 0) {}
            }
            precondition_check($e);
        }
    };
}
fn local_macro(n: usize) { check_shape!(n); }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("local_fn", "a block's fn is in scope in its block"),
    ("local_fn_before", "a block's fn is in scope before its declaration"),
    ("local_const", "a block's const"),
    ("local_struct", "a block's struct and its inherent impl"),
    ("local_use", "a block's use declaration"),
    ("local_shadows", "a block's fn shadows the module's of the same name"),
    ("local_macro", "core's `assert_unsafe_precondition!` shape: an expansion's block declares a const fn and calls it"),
];

/// The bodies that must be refused, with the finding each must have.
const REFUSED: &[(&str, &str)] = &[("out_of_block", "UnresolvedPath")];

/// Library fns that use `assert_unsafe_precondition!`, by path from their crate's root.
const LIBRARY: &[(&str, &[&str])] = &[("core", &["slice", "from_raw_parts"])];

#[test]
fn items_inside_bodies_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/locs/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "locs", dir: "/locs/", root: "/locs/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut front = Front2::new(&names, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"));
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(Alloc::global());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let mut cases: Vec<(String, blaze_id::ItemKey, &str, Option<&str>)> = Vec::new();
    for &(n, p) in CLEAN {
        cases.push((n.to_string(), find(0, &[n]), p, None));
    }
    for &(n, w) in REFUSED {
        cases.push((n.to_string(), find(0, &[n]), "must be refused", Some(w)));
    }
    for &(krate, path) in LIBRARY {
        let ix = crates.iter().position(|c| c.name == krate).unwrap_or_else(|| panic!("{krate} among the crates"));
        cases.push((format!("{krate}::{}", path.join("::")), find(ix, path), "a library fn calling `assert_unsafe_precondition!`", None));
    }
    for (name, key, proves, want) in cases {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            report.push(format!("{name}: no head: {e:?}"));
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => b,
            Err(e) => {
                report.push(format!("{name}: does not lower: {e}"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, &body, &scope), want) {
            (Ok(_), None) => report.push(format!("{name}: clean")),
            (Err(CheckFail::Diags { diags, .. }), Some(w)) if format!("{diags:?}").contains(&format!("({w},")) => {
                report.push(format!("{name}: clean (refused with {w})"));
            }
            (Ok(_), Some(w)) => {
                report.push(format!("{name}: checked clean, but must be refused with {w}"));
                failed.push(name);
            }
            (Err(CheckFail::Diags { diags, .. }), _) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            (Err(other), _) => {
                report.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name);
            }
        }
    }
    // Failures first: the harness's runner shows the first lines only.
    report.sort_by_key(|l| l.contains(": clean"));
    for line in &report {
        println!("{line}");
    }
    let why: Vec<String> = report.iter().filter(|l| !l.contains(": clean")).map(|l| l.chars().take(300).collect()).collect();
    assert!(failed.is_empty(), "bodies that did not check as they must: {failed:?}\n{}", why.join("\n"));
}

//! Items a macro expansion in statement position declares in a block, through the real front
//! end with the installed toolchain (`$HOME/.blaze/v2/toolchain`, as tests/front2_block_mods.rs):
//! each fixture is a fn whose BODY is checked by front2 and must check clean.
//!
//! The Reference, "Statements", "Item declarations" [statement.item.scope]: "Declaring an item
//! within a statement block restricts its scope to the block containing the statement."
//! "Macros", "Macro invocation": a macro may be invoked as a statement, and its expansion is
//! statements. "Macros by example", "Hygiene": "`macro_rules` macros have mixed-site hygiene
//! ... other symbols are looked up at the macro invocation site." So the `mod` an expansion
//! writes in a block (the shape of `tokio::select!`, whose expansion is `{ mod
//! __tokio_select_util { .. } use .. ; .. match .. }`) is an item of that block, and
//! `inner::K` in the same expansion resolves to it. front2 reads which module a block is from
//! the body's ONE scope table (front2/scope_table.rs, where the lookup order is stated).
//!
//! The crate's own `mod inner` has `K: bool`, and its own `mod __inner` has `E::A(bool)`: a
//! fix that reads an expansion's path at the calling block's module (or the body's) finds
//! the crate's `bool` through the block modules' `use super::*`, assigns it to a `u8` and is
//! refused with a mismatch. One that finds no module refuses the local `mod`
//! (`LocalKind::unplaced`).
//!
//! The cases:
//! - `macro_block_mod`: an expansion's block declares `mod inner`; `inner::K` is its u8.
//! - `select_shaped`: a mod, a `use` of an enum from it, and a match on the enum, all the
//!   MACRO's tokens (only `out` is the caller's): `Out::A(5)`, `Out::A(x)`, `Out::B` resolve in
//!   the expansion's block module, through its `use __select_util::Out`.
//! - `nested_calls`: a macro call INSIDE another macro's expansion, whose own expansion writes
//!   a block with `mod __inner`. Both calls have one site in the item (the outer call's
//!   range). A lookup by "the statement call whose range holds the site" finds the OUTER call,
//!   whose blocks do not declare `__inner`, and reads the crate's `__inner::E::A(bool)`.
//! - `twice_called`: two calls of one macro with the same tokens in one block. They have ONE
//!   `blaze_expand::call_key` (one kept expansion), but Rust makes two blocks, two modules and
//!   two `Out` enums. A table keyed by the call key has two candidate modules for one key; the
//!   test also asks that the two expansions' blocks are two different modules.
//! - `dispatch::written_alias`: the expansion's `Calls = u8` is visible before its declaration
//!   and as the receiver in `<Calls as Read>::byte()`. In the same tuple, `super::Parent`
//!   refers to the crate root's `bool`.
//!
//! Expected lines (in any order): `macro_block_mod: clean`, `select_shaped: clean`,
//! `nested_calls: clean`, `twice_called: clean`, then `twice_called: 2 expansion block
//! modules`, and `dispatch::written_alias: clean`. The planted bug (the table answers the
//! calling block's module for every block an
//! expansion wrote, `scope_table::Planted::CallingModuleForExpansions`) must make each of the
//! five NOT clean; that test prints `planted: caught in 5 of 5`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::scope_table::{Planted, ScopeAt};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::ScopeId;
use blaze_lower::scope_origin::ScopeOrigin;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
mod inner { pub const K: bool = true; }
mod __inner { pub enum E { A(bool) } }
macro_rules! set_k {
    ($out:ident) => {{
        mod inner { pub const K: u8 = 7; }
        $out = inner::K;
    }};
}
macro_rules! select_like {
    ($out:ident) => {{
        mod __select_util { pub enum Out { A(u8), B } }
        use __select_util::Out;
        let v = Out::A(5);
        match v {
            Out::A(x) => $out = x,
            Out::B => {}
        }
    }};
}
macro_rules! outer_like {
    ($out:ident) => {{
        mod __outer { pub const K: u8 = 1; }
        inner_like!($out);
        $out += __outer::K;
    }};
}
macro_rules! inner_like {
    ($out:ident) => {{
        mod __inner { pub enum E { A(u8) } }
        use __inner::E;
        let E::A(y) = E::A(3);
        $out = y;
    }};
}
fn macro_block_mod() -> u8 { let mut out = 0; set_k!(out); out }
fn select_shaped() -> u8 { let mut out = 0; select_like!(out); out }
fn nested_calls() -> u8 { let mut out = 0; outer_like!(out); out }
fn twice_called() -> u8 { let mut out = 0; select_like!(out); select_like!(out); out }
type Parent = bool;
mod dispatch {
    type Parent = u32;
    type Calls = bool;
    trait Read { fn byte() -> u8; }
    impl Read for u8 { fn byte() -> u8 { 7 } }
    macro_rules! decode {
        () => {{
            let pair: (Calls, super::Parent) = (7u8, true);
            type Calls = u8;
            <Calls as Read>::byte() + pair.0
        }};
    }
    pub fn written_alias() -> u8 { decode!() }
}
macro_rules! missing { () => {{ let _: Absent = (); }}; }
fn missing_written_type() { missing!() }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("macro_block_mod", "a mod an expansion declares in a block is the block's: `inner::K` is the expansion's u8"),
    ("select_shaped", "a select!-shaped expansion: its mod, a use of an enum from it and a match on the enum"),
    ("nested_calls", "a call inside an expansion: its expansion's block is a module under the INNER call, keyed under the outer expansion's block"),
    ("twice_called", "two equal calls in one block: two blocks, two modules, two enums, by where each call stands"),
    ("dispatch::written_alias", "the block alias shadows the module alias throughout the block, including in a qualified path; super names the enclosing Rust module's parent"),
];

/// One body's verdict: its line, whether it checked clean, and (for `twice_called`) how many
/// distinct own modules the scope table gave the blocks its expansions wrote.
struct Verdict {
    name: &'static str,
    clean: bool,
    line: String,
    expansion_modules: usize,
    type_misses: Vec<(u32, u32, String)>,
}

/// Checks the selected bodies through front2, with the scope table's planted bug `planted`.
fn run(planted: Planted, cases: &[(&'static str, &str)]) -> Vec<Verdict> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/locs/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "locs", dir: "/locs/", root: "/locs/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    store.locals.plant(planted);
    let heap = blaze_own::testing::heap();
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut out = Vec::new();
    for &(name, proves) in cases {
        let path: Vec<_> = name.split("::").collect();
        let key = find(0, &path);
        let refused = |line: String| Verdict { name, clean: false, line, expansion_modules: 0, type_misses: Vec::new() };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            out.push(refused(format!("{name}: no head: {e:?}")));
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => b,
            Err(e) => {
                out.push(refused(format!("{name}: does not lower: {e}")));
                continue;
            }
        };
        let view = body.lowered.view();
        // The distinct own modules of the blocks an expansion wrote, by the body's table.
        let mut modules = Vec::new();
        if let Some(locals) = store.locals.body(body.owner) {
            for (s, at) in locals.scopes.iter().enumerate() {
                let written_by_expansion = matches!(view.scope_origin(ScopeId(s as u32)), Ok(ScopeOrigin::Expansion { .. }));
                if let (true, ScopeAt::Own(m)) = (written_by_expansion, *at)
                    && !modules.contains(&m)
                {
                    modules.push(m);
                }
            }
        }
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let (clean, line) = match check_body(&mut records, &body, &scope) {
            Ok(_) => (true, format!("{name}: clean")),
            Err(CheckFail::Diags { diags, .. }) => (false, format!("{name}: diagnostics {diags:?} ({proves})")),
            Err(other) => (false, format!("{name}: {other:?} ({proves})")),
        };
        let type_misses = body.type_misses.iter().map(|m| (m.at.start, m.at.end, m.name.to_string())).collect();
        out.push(Verdict { name, clean, line, expansion_modules: modules.len(), type_misses });
    }
    out
}

#[test]
fn mods_written_by_expansions_in_blocks_through_front2() {
    let verdicts = run(Planted::None, CLEAN);
    let mut report: Vec<String> = verdicts.iter().map(|v| v.line.clone()).collect();
    let mut failed: Vec<&str> = verdicts.iter().filter(|v| !v.clean).map(|v| v.name).collect();
    // Two equal calls in one block: each expansion's block is its own module.
    if let Some(twice) = verdicts.iter().find(|v| v.name == "twice_called") {
        report.push(format!("twice_called: {} expansion block modules", twice.expansion_modules));
        if twice.expansion_modules != 2 {
            failed.push("twice_called (modules)");
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

/// The planted bug: the scope table answers the calling block's module for a block an
/// expansion wrote. Each case reads the crate's `bool` item (a mismatch) or finds no module
/// for the expansion's `mod` (refused): none checks clean. A test that passed with it would
/// prove nothing about the table.
#[test]
fn the_calling_block_for_an_expansion_block_is_caught() {
    let verdicts = run(Planted::CallingModuleForExpansions, CLEAN);
    let caught = verdicts.iter().filter(|v| !v.clean).count();
    println!("planted: caught in {caught} of {}", verdicts.len());
    let missed: Vec<&str> = verdicts.iter().filter(|v| v.clean).map(|v| v.name).collect();
    assert!(missed.is_empty(), "the planted calling-block answer checked clean in: {missed:?}");
}

/// The printed invocation cannot supply the missing spelling. The path supplies the name
/// and the invocation supplies the item-relative range.
#[test]
fn a_missing_expanded_type_names_its_segment_at_the_call() {
    let verdicts = run(Planted::None, &[("missing_written_type", "an undeclared type in an expansion")]);
    let result = &verdicts[0];
    let item = "fn missing_written_type() { missing!() }";
    let start = item.find("missing!()").unwrap() as u32;
    let [(from, to, name)] = result.type_misses.as_slice() else { panic!("one type miss: {}", result.line) };
    assert_eq!(name, "Absent");
    assert_eq!(*from, start);
    assert_eq!(&item[*from as usize..*to as usize], "missing!()");
    assert!(result.line.contains("cannot find type `Absent`"), "{}", result.line);
    println!("missing_written_type: cannot find type `Absent` at missing!()");
}

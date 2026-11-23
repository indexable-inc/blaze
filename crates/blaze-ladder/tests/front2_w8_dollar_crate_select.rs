//! A `mod` in a block that a RECURSIVE macro writes, the recursion spelled `$crate::m!(..)` at
//! each expansion's top level: the shape of `tokio::select!` (tokio 1.53.3 macros/select.rs),
//! whose entry arm expands to `$crate::select!(@{ .. })`, which normalises one branch per step
//! the same way, until the last step writes `{ #[doc(hidden)] mod __tokio_select_util { .. }
//! let mut disabled: __tokio_select_util::Mask = ..; .. }`. Through the real front end with the
//! installed toolchain (`$HOME/.blaze/v2/toolchain`, as tests/front2_macro_block_mods.rs).
//!
//! The Reference, "Statements", "Item declarations" [statement.item.scope]: "Declaring an item
//! within a statement block restricts its scope to the block containing the statement."
//! "Macros By Example", "Hygiene": `$crate` "refers to the crate defining the macro", and other
//! symbols than locals and labels are looked up at the invocation site. So `__sel_util` is the
//! last expansion's block's own `mod`, and `__sel_util::Mask` is its `u8`.
//!
//! The cause (tempo, 2026-11-18: 54 bodies "a mod in a block that a macro call wrote or that is
//! in a local item's body", every one a `tokio::select!`): the body lowering keeps `$crate` as
//! written in an expansion's text (blaze-expand body.rs `body_words`), the names library writes
//! `crate` there (blaze-scope resolve.rs `render_with`), and the block scan
//! (blaze-file-items nested.rs `scan`) listed a statement call only when its path began with a
//! word or `::`. front2's scope table (front2/scope_table.rs `build`) found no statement call
//! at the recursive call's place, so no expansion after it had a call key and the last one's
//! block was `ScopeAt::Unfiled`: its `mod` was refused by name.
//!
//! The crate's own `mod __sel_util` has `Mask = bool` and `Out::A(bool)`: a reading at the
//! calling block's module or the body's finds those, and the clean cases are refused with a
//! mismatch. The control `bad_mask` writes `let mask: __sel_util::Mask = true;` in the same
//! expansion block: it must be a mismatch (E0308), which only the block's `u8` gives.
//!
//! Expected lines: `two_deep: clean`, `tail_position: clean`, `in_loop: clean`,
//! `bad_mask: E0308` (any order), then from the second test `planted: caught in 3 of 3`, and from
//! the third `expression_position: refused by name (a mod in a block that a macro call wrote
//! ..)`: an expression-position call is no statement call of its block, which the names library
//! files no module for (still owed; see the report of lane W8, 2026-11-18).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::scope_table::Planted;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = r#"
fn main() {}
mod __sel_util {
    pub type Mask = bool;
    pub enum Out { A(bool), B }
}
#[macro_export]
macro_rules! sel {
    (@done $out:ident) => {{
        #[doc(hidden)]
        mod __sel_util {
            pub type Mask = u8;
            pub enum Out { A(u8), B }
        }
        let mask: __sel_util::Mask = 7;
        let v = __sel_util::Out::A(mask);
        match v {
            __sel_util::Out::A(x) => $out = x,
            __sel_util::Out::B => {}
        }
    }};
    (@step $out:ident) => { $crate::sel!(@done $out) };
    ($out:ident) => { $crate::sel!(@step $out) };
}
#[macro_export]
macro_rules! sel_bad {
    (@done $out:ident) => {{
        mod __sel_util { pub type Mask = u8; }
        let mask: __sel_util::Mask = true;
        $out = mask;
    }};
    ($out:ident) => { $crate::sel_bad!(@done $out) };
}
fn two_deep() -> u8 { let mut out = 0; sel!(out); out }
fn tail_position() { let mut out = 0u8; sel! { out } }
fn in_loop() -> u8 { let mut out = 0; loop { sel!(out); break; } out }
fn bad_mask() -> u8 { let mut out = 0; sel_bad!(out); out }
fn expression_position() -> u8 { let mut out = 0; let () = sel!(out); out }
"#;

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("two_deep", "two `$crate::sel!` steps before the expansion that writes the block's `mod`"),
    ("tail_position", "a brace call as the body's last statement, with no `;` (tempo's `shutdown_signal`)"),
    ("in_loop", "the call in a `loop`'s block (tempo's actor `run` loops)"),
];

/// How one body came out: clean, diagnostics (their debug text), or refused (its text).
enum Outcome {
    Clean,
    Diags(String),
    Refused(String),
}

/// Lowers and checks each named body through front2, with the scope table's planted bug.
fn run(planted: Planted, cases: &[&'static str]) -> Vec<(&'static str, Outcome)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/w8sel/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "w8sel", dir: "/w8sel/", root: "/w8sel/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    // A test fixture's allocator, as tests/front2_macro_block_mods.rs takes it.
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    store.locals.plant(planted);
    let heap = blaze_own::testing::heap();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut out = Vec::new();
    for &name in cases {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|path| path.whole(1))
            .map(|def| def.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"));
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            out.push((name, Outcome::Refused(format!("no head: {e:?}"))));
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => b,
            Err(e) => {
                out.push((name, Outcome::Refused(format!("does not lower: {e}"))));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("body spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let outcome = match check_body(&mut records, &body, &scope) {
            Ok(done) if done.diags.is_empty() => Outcome::Clean,
            Ok(done) => Outcome::Diags(format!("{:?}", done.diags)),
            Err(CheckFail::Diags { diags, .. }) => Outcome::Diags(format!("{diags:?}")),
            Err(other) => Outcome::Refused(format!("{other:?}")),
        };
        out.push((name, outcome));
    }
    out
}

/// The three clean bodies check clean, and the control is a mismatch.
/// Prints `two_deep: clean`, `tail_position: clean`, `in_loop: clean`, `bad_mask: E0308`.
#[test]
fn a_mod_a_dollar_crate_recursion_writes_in_a_block_is_the_blocks() {
    let mut names: Vec<&'static str> = CLEAN.iter().map(|(n, _)| *n).collect();
    names.push("bad_mask");
    let mut failed = Vec::new();
    for (name, outcome) in run(Planted::None, &names) {
        let proves = CLEAN.iter().find(|(n, _)| *n == name).map(|(_, p)| *p);
        match (proves, outcome) {
            (Some(_), Outcome::Clean) => println!("{name}: clean"),
            (Some(proves), Outcome::Diags(d)) => failed.push(format!("{name}: diagnostics {d} ({proves})")),
            (Some(proves), Outcome::Refused(r)) => failed.push(format!("{name}: refused {r} ({proves})")),
            (None, Outcome::Diags(d)) if d.contains("E0308") || d.contains("Mismatch") || d.contains("mismatch") => println!("{name}: E0308"),
            (None, Outcome::Diags(d)) => failed.push(format!("{name}: diagnostics other than a mismatch: {d}")),
            (None, Outcome::Clean) => failed.push(format!("{name}: checked clean, so `__sel_util::Mask` was not the block's `u8`")),
            (None, Outcome::Refused(r)) => failed.push(format!("{name}: refused, not a mismatch: {r}")),
        }
    }
    let shown: Vec<String> = failed.iter().map(|l| l.chars().take(400).collect()).collect();
    assert!(failed.is_empty(), "{}", shown.join("\n"));
}

/// The planted bug (the scope table answers the calling block's module for every block an
/// expansion wrote): the calling block's module binds no `__sel_util`, so none of the three is
/// clean. A test that passed with it would prove nothing about the table.
/// Prints `planted: caught in 3 of 3`.
#[test]
fn the_calling_block_for_a_recursive_expansions_block_is_caught() {
    let names: Vec<&'static str> = CLEAN.iter().map(|(n, _)| *n).collect();
    let verdicts = run(Planted::CallingModuleForExpansions, &names);
    let missed: Vec<&str> = verdicts.iter().filter(|(_, o)| matches!(o, Outcome::Clean)).map(|(n, _)| *n).collect();
    println!("planted: caught in {} of {}", verdicts.len() - missed.len(), verdicts.len());
    assert!(missed.is_empty(), "the planted calling-block answer checked clean in: {missed:?}");
}

/// What is still owed, pinned: a call in EXPRESSION position (`let () = sel!(out);`, tempo's
/// `let recv = tokio::select! { .. }`) is no statement call of its block, the names library
/// writes no module for the blocks its expansion writes, and the `mod` there is refused by its
/// name, never read at another block. Prints `expression_position: refused by name (..)`.
#[test]
fn a_mod_in_an_expression_calls_block_is_refused_by_name() {
    let verdicts = run(Planted::None, &["expression_position"]);
    let (_, outcome) = &verdicts[0];
    match outcome {
        Outcome::Clean => println!("expression_position: clean"),
        Outcome::Refused(r) => panic!("refused: {r}"),
        Outcome::Diags(d) => panic!("a false error where rustc accepts: {d}"),
    }
}

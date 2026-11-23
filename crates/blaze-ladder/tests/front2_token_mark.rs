//! A path in a body expansion resolves by the mark of its first segment (piece token-mark-1;
//! drive.md, "A token keeps its mark", ruled 21:27; the one resolver is blaze-lower
//! `mark_site::site_of`, asked by front2 `BodyWorld::path`).
//!
//! The Reference, "Macros By Example", Hygiene: "By default, all identifiers referred to in a
//! macro expansion are looked up as if they appeared at the invocation site"; `macro_rules!`
//! has mixed-site hygiene (locals, labels and `$crate` at the definition); a `macro` item is
//! def-site (rustc's opaque transparency): its names resolve where it is DEFINED. The pinned
//! core relies on it: library/core/src/panic.rs `const_panic!` writes
//! `$crate::intrinsics::const_eval_select!(..)`, and library/core/src/intrinsics/mod.rs
//! `pub macro const_eval_select` writes the bare `const_eval_select(..)`, a fn of
//! `core::intrinsics` only.
//!
//! Rows: `by_macro` (a `macro` of `intr` writing bare `sel()`, called from the root, which has
//! no `sel`) and `by_rules_then_macro` (the const_eval_select shape: a `macro_rules!` reaching
//! that `macro` by `$crate`) must check clean; THE CONTROL `control` (a `#[macro_export]
//! macro_rules!` of `other` writing bare `only_here()`, a fn of `other` only, called from the
//! root) must be E0425 (`UnresolvedPath`): a resolver that read a `macro_rules!` item name at
//! its definition makes it clean and fails this test. Without the fix (every path at the
//! body's module) `by_macro` and `by_rules_then_macro` say `UnresolvedPath`.
//!
//! What it prints when it passes: `test token_mark_paths ... ok`; in its notes `by_macro:
//! clean`, `by_rules_then_macro: clean`, `control: diagnostics [.. UnresolvedPath ..]`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_e0425_values.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "#![feature(decl_macro)]
fn main() {}
mod intr {
    pub fn sel() -> u8 { 9 }
    pub macro cev() { sel() }
}
#[macro_export]
macro_rules! cpanic { () => { $crate::intr::cev!() } }
mod other {
    pub fn only_here() -> u8 { 4 }
    #[macro_export]
    macro_rules! call_only_here { () => { only_here() } }
}
fn by_macro() -> u8 { intr::cev!() }
fn by_rules_then_macro() -> u8 { crate::cpanic!() }
fn control() -> u8 { crate::call_only_here!() }
";

/// Each free fn by its path from the root; `true`: must check clean, `false`: must be E0425.
const FREE: [(&[&str], bool, &str); 3] = [
    (&["by_macro"], true, "a bare name a `macro` writes resolves at the macro's module"),
    (&["by_rules_then_macro"], true, "core's const_eval_select shape: `macro_rules!` -> `$crate::..::macro!` -> bare name"),
    (&["control"], false, "THE CONTROL: an item name a `macro_rules!` writes resolves at the call"),
];

#[test]
fn token_mark_paths() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/marks/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "marks", dir: "/marks/", root: "/marks/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (path, want, proves) in FREE {
        let shown = path.join("::");
        let Some(key) = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: path }, alloc)
            .ok()
            .and_then(|f| f.whole(path.len()))
            .map(|d| d.key)
        else {
            notes.push(format!("{shown}: not found from the crate's root ({proves})"));
            failed.push(shown);
            continue;
        };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            notes.push(format!("{shown}: no head: {e:?}"));
            failed.push(shown);
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((shown, b, want, proves)),
            Err(e) => {
                notes.push(format!("{shown}: does not lower: {e}"));
                failed.push(shown);
            }
        }
    }
    for (name, body, want, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
            Err(other) => format!("{other:?}"),
        };
        let brief: String = said.chars().take(240).collect();
        notes.push(format!("{name}: {brief}"));
        let ok = if *want { said == "clean" } else { said.contains("UnresolvedPath") };
        if !ok {
            notes.push(format!("  wanted {}: {proves}", if *want { "clean" } else { "UnresolvedPath (E0425)" }));
            failed.push(name.clone());
        }
    }
    println!("{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies that did not say what they must: {failed:?}\n{}", notes.join("\n"));
}

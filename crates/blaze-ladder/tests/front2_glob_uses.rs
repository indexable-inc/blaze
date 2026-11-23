//! A glob `use` declared in a block, through the real front end with the installed toolchain
//! (`$HOME/.blaze/v2/toolchain`, as tests/front2_locals.rs): each fixture is a fn whose BODY is
//! checked by front2 and must check clean, or be refused with the finding named.
//!
//! The Reference (the pinned toolchain's copy, `share/doc/rust/html/reference`):
//! "Use declarations" (items/use-declarations.html): "Use declarations may appear in modules
//! and blocks, usually at the top." "Glob imports": "The `*` character may be used as the last
//! segment of a `use` path to import all importable entities from the entity of the preceding
//! segment." and "Items and named imports are allowed to shadow names from glob imports in the
//! same namespace. That is, if there is a name already defined by another item in the same
//! namespace, the glob import will be shadowed."
//! "Scopes" (names/scopes.html): "It is an error to introduce an item with a duplicate name of
//! another item in the same namespace within the same module or block. Asterisk glob imports
//! have special behavior for dealing with duplicate names and shadowing".
//! "Name resolution" (names/name-resolution.html), [names.resolution.expansion.imports.shadowing]:
//! "Names introduced via use declarations in an outer scope are shadowed by candidates in the
//! same namespace with the same name from an inner scope except where otherwise restricted by
//! name resolution ambiguities." [..ambiguity.glob-vs-glob]: "Names may not be resolved
//! through ambiguous glob imports. Glob imports are allowed to import conflicting names in the
//! same namespace as long as the name is not used. [..] The errors occur at time of use, not
//! time of import." and "Multiple glob imports are allowed to import the same name, and that
//! name is allowed to be used if the imports are of the same item (following reexports)."
//! "Identifier patterns" (patterns.html): "Path patterns take precedence over identifier
//! patterns": after `use self::E::*;` the pattern `A` is the variant `E::A`, never a binding.
//!
//! What each fixture would do under a wrong fix: `variant_not_binding` checks CLEAN if `A`
//! binds (a binding matches a `u8`); `block_item_wins` and `glob_shadows_module` are a
//! mismatch if the order of the block's item, the block's glob and the module is wrong;
//! `ambiguous_used` is clean if the first glob is taken; `ambiguous_unused` and `same_item`
//! are refused if an ambiguity is raised at the import.
//!
//! The test prints, in this order (failures would come first):
//! ```text
//! glob_match: clean
//! glob_shadows_module: clean
//! block_item_wins: clean
//! ambiguous_unused: clean
//! same_item: clean
//! local_enum: clean
//! child::sees: clean
//! glob_wrong_arm: clean (refused with Mismatch)
//! variant_not_binding: clean (refused with Mismatch)
//! ambiguous_used: clean (refused with the gap: a name that two glob uses of one block import as two different items (ambiguous, E0659))
//! ```

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::glob_uses::AMBIGUOUS;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::world_items::Gap;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
enum E { A, B(u8) }
mod m1 { pub fn both() -> u8 { 1 } pub fn one() -> u8 { 1 } pub use super::shared; }
mod m2 { pub fn both() -> u8 { 2 } pub use super::shared; }
fn one() -> bool { true }
pub fn shared() -> u8 { 9 }
fn parent_fn() -> u8 { 7 }
fn glob_match(e: E) -> u8 { use self::E::*; let made = B(1); match made { A => 0, B(_) => 0 }; match e { A => 0, B(x) => x } }
fn glob_wrong_arm(e: E) -> u8 { use self::E::*; match e { A => true, B(x) => x } }
fn variant_not_binding(n: u8) -> u8 { use self::E::*; match n { A => 0, _ => 1 } }
fn glob_shadows_module() -> u8 { use self::m1::*; one() }
fn block_item_wins() -> bool { use self::m1::*; fn one() -> bool { true } one() }
fn ambiguous_used() -> u8 { use self::m1::*; use self::m2::*; both() }
fn ambiguous_unused() -> u8 { use self::m1::*; use self::m2::*; one() }
fn same_item() -> u8 { use self::m1::*; use self::m2::*; shared() }
fn local_enum() -> u8 { enum K { P, Q } use K::*; let k = P; match k { P => 1, Q => 2 } }
mod child { pub fn sees() -> u8 { let n = { use super::*; parent_fn() }; n } }
";

/// The bodies that must check clean, by path from the crate's root, each with its rule.
const CLEAN: &[(&[&str], &str)] = &[
    (&["glob_match"], "`use self::E::*;`: `A` and `B(x)` are patterns of the variants and `B(1)` constructs one"),
    (&["glob_shadows_module"], "a block's glob import shadows the module's item of the same name (m1's `one` is a `u8`)"),
    (&["block_item_wins"], "a block's own item shadows the block's glob import (its `one` is a `bool`)"),
    (&["ambiguous_unused"], "two globs that import `both` as two items, `both` not used"),
    (&["same_item"], "two globs that import one item under one name"),
    (&["local_enum"], "a glob import of a block's own enum"),
    (&["child", "sees"], "`use super::*;` in a nested block imports the parent module's fn"),
];

/// The bodies that must be refused with a diagnostic, by its code.
const REFUSED: &[(&[&str], &str)] = &[(&["glob_wrong_arm"], "Mismatch"), (&["variant_not_binding"], "Mismatch")];

/// The bodies that must stop at the gap of this form.
const GAPS: &[(&[&str], &str)] = &[(&["ambiguous_used"], AMBIGUOUS)];

/// What a case must do.
#[derive(Clone, Copy)]
enum Must {
    Clean,
    Diag(&'static str),
    Gap(&'static str),
}

#[test]
fn glob_uses_inside_bodies_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/globs/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "globs", dir: "/globs/", root: "/globs/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let find = |segments: &[&str]| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from the crate's root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let mut cases: Vec<(String, blaze_id::ItemKey, &str, Must)> = Vec::new();
    for &(path, proves) in CLEAN {
        cases.push((path.join("::"), find(path), proves, Must::Clean));
    }
    for &(path, code) in REFUSED {
        cases.push((path.join("::"), find(path), "must be refused", Must::Diag(code)));
    }
    for &(path, form) in GAPS {
        cases.push((path.join("::"), find(path), "must stop at the gap", Must::Gap(form)));
    }
    for (name, key, proves, must) in cases {
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
        match (check_body(&mut records, &body, &scope), must) {
            (Ok(_), Must::Clean) => report.push(format!("{name}: clean")),
            (Err(CheckFail::Diags { diags, .. }), Must::Diag(w)) if format!("{diags:?}").contains(&format!("({w},")) => {
                report.push(format!("{name}: clean (refused with {w})"));
            }
            (Err(CheckFail::Gap { gaps, .. }), Must::Gap(form)) if gaps.iter().any(|g| matches!(g, Gap::Local(l) if l.form == form)) => {
                report.push(format!("{name}: clean (refused with the gap: {form})"));
            }
            (Ok(_), Must::Diag(w) | Must::Gap(w)) => {
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

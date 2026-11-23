//! A `mod` and an `extern crate` declared in a block, through the real front end with the
//! installed toolchain (`$HOME/.blaze/v2/toolchain`, as tests/front2_locals.rs): each fixture
//! is a fn whose BODY is checked by front2 and must check clean, or be refused with the
//! finding named.
//!
//! The Reference, "Statements", "Item declarations" [statement.item.scope]: "Declaring an item
//! within a statement block restricts its scope to the block containing the statement."
//! That holds for every item kind, a module and an extern crate among them. "Names",
//! "Scopes", item scopes: an item declared in a block is in scope in the whole block and
//! shadows the outer names of its namespace. "Extern crate declarations"
//! [items.extern-crate.intro]: "The external crate is then bound into the declaring scope as
//! the given identifier in the type namespace." "Paths": a path's first segment that names a
//! module continues into that module's items.
//!
//! So: the block's `mod util` and `extern crate core as c` are items of the block's module in
//! the names library (blaze-file-items nested.rs `NestedKind::makes_module`; blaze-scope
//! blocks.rs), a written type in the block is read there (front2 body.rs `env_at`, through
//! `BodyLocals::blocks`), and a body path whose first segment names them is resolved at that
//! module (front2 world_paths.rs). The crate's own `mod util` has `Mask = bool` and
//! `one() -> bool`: a fix that reads the block's `util::Mask` or `util::one` at the body's
//! module checks `let x: bool = 1` or returns a `bool` and is refused with a mismatch, so
//! `block_mod_type` and `block_mod_value` fail without the block's module.
//!
//! Expected lines (in any order): `block_mod_type: clean`, `block_mod_value: clean`,
//! `block_mod_inner_block: clean`, `block_extern_crate_type: clean`,
//! `block_extern_crate_value: clean`, `block_mod_out_of_block: clean (refused with
//! UnresolvedPath)`. Before the change the first five print a `Local` gap naming "a mod in a
//! block" or "an extern crate in a block", or a mismatch.

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
mod util { pub type Mask = bool; pub fn one() -> bool { true } }
fn block_mod_type() -> u8 { mod util { pub type Mask = u8; } let x: util::Mask = 1; x }
fn block_mod_value() -> u8 { mod util { pub fn one() -> u8 { 1 } } util::one() }
fn block_mod_inner_block() -> u8 { mod util { pub type Mask = u8; } { let x: util::Mask = 2; x } }
fn block_extern_crate_type() -> u8 { extern crate core as c; let w: c::num::Wrapping<u8> = c::num::Wrapping(1); w.0 }
fn block_extern_crate_value() -> u8 { extern crate core as c; c::cmp::max(1, 2) }
fn block_mod_out_of_block() -> u8 { { mod inner { pub fn one() -> u8 { 1 } } } inner::one() }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("block_mod_type", "a block's mod shadows the crate's of the same name for a written type"),
    ("block_mod_value", "a block's mod shadows the crate's of the same name for a body path"),
    ("block_mod_inner_block", "a block's mod is in scope in the blocks inside it"),
    ("block_extern_crate_type", "a block's `extern crate core as c` binds `c` for a written type"),
    ("block_extern_crate_value", "a block's `extern crate core as c` binds `c` for a body path"),
];

/// The bodies that must be refused, with the finding each must have: a mod declared in an
/// inner block is not in scope after that block ([statement.item.scope]).
const REFUSED: &[(&str, &str)] = &[("block_mod_out_of_block", "UnresolvedPath")];

#[test]
fn mods_and_extern_crates_in_blocks_through_front2() {
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
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
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

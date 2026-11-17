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
//! `inner::K` in the same expansion resolves to it.
//!
//! The crate's own `mod inner` has `K: bool`: a fix that reads the expansion's `inner::K` at
//! the body's module assigns a `bool` to a `u8` and is refused with a mismatch; without the
//! block's module front2 refuses the local `mod` (`LocalKind::unplaced`), so both fixtures fail
//! without the change.
//!
//! Expected lines (in any order): `macro_block_mod: clean`, `select_shaped: clean`.

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

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
mod inner { pub const K: bool = true; }
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
fn macro_block_mod() -> u8 { let mut out = 0; set_k!(out); out }
fn select_shaped() -> u8 { let mut out = 0; select_like!(out); out }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("macro_block_mod", "a mod an expansion declares in a block is the block's: `inner::K` is the expansion's u8"),
    ("select_shaped", "a select!-shaped expansion: its mod, a use of an enum from it and a match on the enum"),
];

#[test]
fn mods_written_by_expansions_in_blocks_through_front2() {
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
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame);
    let heap = blaze_own::testing::heap();
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(heap.alloc());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    for &(name, proves) in CLEAN {
        let key = find(0, &[name]);
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
        match check_body(&mut records, &body, &scope) {
            Ok(_) => report.push(format!("{name}: clean")),
            Err(CheckFail::Diags { diags, .. }) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            Err(other) => {
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

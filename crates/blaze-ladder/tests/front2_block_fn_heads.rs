//! A fn declared in a body whose HEAD names a type declared beside it in the same block, through
//! the real front end with the installed toolchain (`$HOME/.blaze/v2/toolchain`, as
//! tests/front2_block_mods.rs): each fixture's body is checked by front2 and must check clean.
//!
//! The form is the pinned std's `panicking::catch_unwind`: `union Data<F, R> { .. }` and
//! `unsafe fn do_call<F: FnOnce() -> R, R>(data: *mut Data<F, R>)` are items of one fn body, and
//! checking the body's call of `do_call` needs `do_call`'s head (tempo's build census 7: `LEFT 14
//! check: Head: Item: Resolve(Path(PathError .. NotFound)), Range 64..74`, the text `Data<F, R>`).
//!
//! The Reference, "Statements", "Item declarations" [statement.item.scope]: "Declaring an item
//! within a statement block restricts its scope to the block containing the statement."
//! "Names", "Scopes", item scopes: an item declared in a block is in scope in the whole block,
//! and shadows the outer names of its namespace. So a local fn's parameter types are read at
//! its block's module (front2 locals.rs `LocalHead::names_at`), never at the body's module.
//! The crate's own `struct Pair(bool)` is what a head read at the body's module finds instead:
//! `block_fn_head_names_block_struct` then fails with a mismatch, and
//! `block_fn_head_names_block_generic` with `NotFound` for `Data`.
//!
//! Expected lines (in any order): `block_fn_head_names_block_struct: clean`,
//! `block_fn_head_names_block_generic: clean`. Before the change both print a refusal
//! (`Head` with `Resolve(Path(PathError { .. why: NotFound }))`, or diagnostics).

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
struct Pair(bool);
fn block_fn_head_names_block_struct() -> u8 { struct Pair { a: u8, b: u8 } fn first(p: &Pair) -> u8 { p.a } first(&Pair { a: 1, b: 2 }) }
fn block_fn_head_names_block_generic() -> u8 { struct Data<F> { f: F } fn take<F: Copy>(d: &Data<F>) -> F { d.f } take(&Data { f: 3u8 }) }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("block_fn_head_names_block_struct", "a block fn's parameter type is the block's struct, which shadows the crate's"),
    ("block_fn_head_names_block_generic", "a block fn's parameter type is the block's generic struct, at the fn's own generics"),
];

#[test]
fn block_fn_heads_read_at_their_block() {
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
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}\'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
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

//! A Rust program with std starts through the library's `#[lang = "start"]` (LADDER.md L1a;
//! front2/start.rs).
//!
//! The Rust Reference, "Crates and source files" (`crate.main`): "If a `main` function is
//! present, it must take no arguments, must not declare any trait or lifetime bounds, must not
//! have any where clauses, and its return type must implement the `Termination` trait." The
//! pinned std (nightly-2026-10-27, library/std/src/rt.rs:197) runs it:
//! `#[lang = "start"] fn lang_start<T: crate::process::Termination + 'static>(main: fn() -> T,
//! argc: isize, argv: *const *const u8, sigpipe: u8) -> isize`. So for `fn main() {}` in
//! edition 2024 the start is std's `rt::lang_start` at `T = ()` (`()` is the return type of a
//! fn with none written: Reference, "Functions", "If the output type is not explicitly
//! stated, it is the unit type").
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_suite.rs.
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder test '--test rust_start'`
//! prints "test blaze-ladder: exit 0; 1 passed, 0 failed". Before start.rs it fails to compile
//! (no module `front2::start`); with a `rust_start` that called `main` directly or read the
//! wrong item, the path or the type assertion fails.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::start::{Entry2, linked_lang_start, rust_start};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::PNode;

const MAIN: &str = "fn main() {}\n";

#[test]
fn rust_start_of_hello() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());

    // An edition-2024 root with std, exporting nothing: the Rust start.
    let lang_start = linked_lang_start(&names, blaze_own::testing::heap().alloc())
        .unwrap_or_else(|e| panic!("the start rule: {e}"))
        .expect("an edition-2024 root with std starts through `#[lang = \"start\"]`");
    let written = Written { kind: PathKind::Plain, segments: &["main"] };
    let found = names::path(&names, names.root(0), Namespace::Values, written, Alloc::global()).ok();
    let main = found.and_then(|f| f.whole(1)).expect("the crate root's `fn main` resolves").key;

    let start = rust_start(&names, lang_start, main, &mut store, &mut front.keys).unwrap_or_else(|e| panic!("rust_start: {e}"));
    assert_eq!(start.main, main);
    assert_eq!(start.lang_start, lang_start);
    assert_eq!(Entry2::Rust(start).root(), main, "the walk starts at `main`; `lang_start` is reached beside it");

    // The `#[lang = "start"]` item is std's `rt::lang_start`.
    let lib = start.lang_start;
    let name = names.item_text(lib).and_then(|t| t.name).expect("lang_start has a name");
    let krate = names.crate_of(lib).expect("lang_start is in a crate");
    let module = names.module_of(lib).and_then(|m| names.krate(krate).path_of(m)).expect("lang_start's module");
    let path = format!("{module}::{name}");
    assert!(path.ends_with("rt::lang_start"), "the `#[lang = \"start\"]` item is `{path}`, not std's `rt::lang_start`");

    // `T` is `()`: the empty tuple in the store's table.
    match store.tys.view().get(start.returns) {
        Some(PNode::Tuple(list)) if list.len == 0 => {}
        other => panic!("main's return type is {other:?}, not `()`"),
    }
}

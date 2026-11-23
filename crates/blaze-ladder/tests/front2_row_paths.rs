//! A row's path is DATA built from the names the program declares, never a message printer's
//! fallback (refusals-name-items-2; rule 30, rule 54's class "a display string serving as data").
//!
//! Before: a fn row's path came from check.rs `item_shown`, whose fallback for a key the names
//! library does not name (every item declared in a body) was `format!("{k:?}")`, so a fn
//! declared in a block had the row path `ItemKey(<38 digits>)`, and the symbol code read that
//! path's last segment as data (hello runner 9: "`ItemKey(4952..)`: it takes parameters").
//! Now a row carries front2 row_path.rs `RowPath`: its key and segments, a body-local item's
//! segment being the identifier its declaration binds (the Reference, "Items": "Items may be
//! declared in a block"; the item's name is the identifier after `fn`, [items.fn.syntax]).
//!
//! - `a_body_local_fn_row_is_named_by_its_identifier`: the hosted entry declared in the body of
//!   `launcher` is the run's entry row; the run's error names the row by its path:
//!   `launcher::bstd_hosted_start`, and no text the run prints holds "ItemKey(".
//!   CONTROL (today's text, without the wiring): "HOSTED IN A BODY: `ItemKey(..)` is bstd's
//!   hosted entry, and this run was given no host table to start it with": the first assert
//!   fails.
//! - `a_fill_error_names_its_item_by_path`: `FillError::NoDetail` of `outer::inner::deep` printed
//!   with names (row_path.rs `fill_error_shown`) says the item's path; its `Display` (no names at
//!   hand) said `ItemKey(<12 hex>)`, the crate's prefix only. CONTROL: `format!("{e}")` holds no
//!   `deep`.
//!
//! Prints: `ROW PATHS: local: .. `launcher::bstd_hosted_start` ..` and
//! `ROW PATHS: fill: the names library holds no detail for item <crate>::outer::inner::deep (key ..)`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::fill::FillError;
use blaze_ladder::front2::row_path::fill_error_shown;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

/// front2_hosted_entry_shape.rs's program: the hosted entry is declared in `launcher`'s body.
const HOSTED_IN_A_BODY: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

fn main(code: i32) -> i32 {
    code
}

fn launcher() -> i32 {
    #[unsafe(no_mangle)]
    pub extern \"C\" fn bstd_hosted_start(table: *const u8) -> i32 {
        crate::main(7)
    }
    0
}
";

const EDITION: &str = "blaze-2026.11";
const NESTED: &str = "mod outer { pub mod inner { pub fn deep() {} } }
fn main() { outer::inner::deep() }
";

#[test]
fn a_body_local_fn_row_is_named_by_its_identifier() {
    let files = [SourceFile { path: "src/main.rs", text: HOSTED_IN_A_BODY }];
    let said = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => panic!("ROW PATHS: ran to status {status} with no host table; the hosted entry needs one"),
    };
    println!("ROW PATHS: local: {said}");
    assert!(said.contains("`launcher::bstd_hosted_start`"), "the body-local entry's row is not named by its identifier: {said}");
    assert!(!said.contains("ItemKey("), "a key's Debug reached a row's path: {said}");
}

/// The serde-derive shape: an impl inside `const _: () = { .. }`, its method called by `main`.
/// The method's row path walks up through the impl (declared in the const's body) to the const,
/// which has no name: its segment is `{const}` (refusals-name-items-3), never a refusal.
const IN_AN_UNNAMED_CONST: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

struct S;

const _: () = {
    impl S {
        fn show(&self) -> i32 {
            0
        }
    }
};

fn main() {
    let _ = S.show();
}
";

/// CONTROL (refusals-name-items-2 alone): the run stops with "front2: no row path is built for
/// .. an item of its path has no name and is no impl" (the tempo census's stop, run 3, 21:00).
/// Whatever else the run says (a no_core program may stop later for its own reasons), it never
/// says that.
#[test]
fn a_fn_in_an_unnamed_const_has_a_row_path() {
    let files = [SourceFile { path: "src/main.rs", text: IN_AN_UNNAMED_CONST }];
    let said = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => format!("ran to status {status}"),
    };
    println!("ROW PATHS: const _: {said}");
    assert!(!said.contains("no row path is built"), "a nameless const stopped a row path: {said}");
    assert!(!said.contains("ItemKey("), "a key's Debug reached a message: {said}");
}

#[test]
fn a_fill_error_names_its_item_by_path() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), NESTED.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: NESTED }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let segments = ["outer", "inner", "deep"];
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &segments }, alloc.clone())
        .unwrap_or_else(|e| panic!("`outer::inner::deep`: {e}"));
    let k = found.whole(segments.len()).unwrap_or_else(|| panic!("`outer::inner::deep` not named")).key;
    let e = FillError::NoDetail(k);
    let shown = fill_error_shown(&names, &front.keys, &e);
    println!("ROW PATHS: fill: {shown}");
    let tail: String = k.0.0[8..14].iter().map(|b| format!("{b:02x}")).collect();
    assert!(shown.ends_with(&format!("outer::inner::deep (key {tail})")), "{shown}");
    assert!(!format!("{e}").contains("deep"), "control: FillError's Display holds no names");
}

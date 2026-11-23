//! Every key a row path meets is named by the names source's table that holds it
//! (refusals-name-items-5; rule 54: "a nameless item always has a kind" was the census's
//! reading, the cause is that a VARIANT has no item row at all, so no name, text or kind:
//! front2 source.rs `has_text`: "a variant is not [an item with text]").
//!
//! - `a_variant_ctor_has_a_row_path`: `main` calls the tuple variant `E::Two` as a ctor
//!   (`Reached::Ctor`). Its row path is built from the enum's variant list (the Reference,
//!   "Enumerations": a tuple-like variant's name is also a constructor fn in the value
//!   namespace, [items.enum.constructor-names]): the run never says "no row path is built".
//!   CONTROL: without the wiring's edit 1 it says "front2: no row path is built for .. no kind
//!   in the names source" (the build census run 5's class, 1,295 std fns).
//! - `a_variant_is_shown_by_its_path`: `ItemShown` of `E::Two` is `<crate>::E::Two (key ..)`.
//!   CONTROL: without edit 2, "an item the names source holds no path for (key <32 hex>)".
//!
//! Prints: `ROW PATH FORMS: variant: <the run's result>` and
//! `ROW PATH FORMS: variant shown: <crate>::E::Two (key <12 hex>)`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::item_shown::ItemShown;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const A_VARIANT_CTOR: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

enum E {
    One,
    Two(i32),
}

fn main() {
    let _ = E::Two(2);
}
";

const EDITION: &str = "blaze-2026.11";
const AN_ENUM: &str = "pub enum E { One, Two(i32) }
fn main() { let _ = E::Two(2); }
";

#[test]
fn a_variant_ctor_has_a_row_path() {
    let files = [SourceFile { path: "src/main.rs", text: A_VARIANT_CTOR }];
    let said = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => format!("ran to status {status}"),
    };
    println!("ROW PATH FORMS: variant: {said}");
    assert!(!said.contains("no row path is built"), "a variant's row path was refused: {said}");
    assert!(!said.contains("ItemKey("), "a key's Debug reached a message: {said}");
}

#[test]
fn a_variant_is_shown_by_its_path() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), AN_ENUM.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: AN_ENUM }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let _front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let segments = ["E", "Two"];
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &segments }, alloc.clone())
        .unwrap_or_else(|e| panic!("`E::Two`: {e}"));
    let k = found.whole(segments.len()).unwrap_or_else(|| panic!("`E::Two` not named")).key;
    let shown = ItemShown::of_lib(&names, k).to_string();
    println!("ROW PATH FORMS: variant shown: {shown}");
    let tail: String = k.0.0[8..14].iter().map(|b| format!("{b:02x}")).collect();
    assert!(shown.ends_with(&format!("E::Two (key {tail})")), "{shown}");
}

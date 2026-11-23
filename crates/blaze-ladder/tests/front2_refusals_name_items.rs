//! A refusal names its item by its PATH (rule 19), never by a key's short form, which is its
//! CRATE's part (blaze-id lib.rs `impl Display for ItemKey`: the first 12 hex digits, bytes
//! 0..6, are the crate prefix) or a checker key's 38-digit `Debug`. Two fixtures over the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`, built like
//! tests/front2_unread_impl_path.rs:
//!
//! - `a_nested_item_is_shown_by_its_path`: `outer::inner::deep`, by its library key and by its
//!   checker key, prints `<crate>::outer::inner::deep (key <bytes 8..14>)`
//!   (front2/item_shown.rs `ItemShown`). Before it there was no such printer: a refusal's
//!   `{k:?}` printed `ItemKey(<crate prefix>)`, the same for every item of the crate.
//! - `a_needed_head_names_its_item_and_the_header_that_needed_it`: the header of
//!   `impl Local<outer::inner::Bad> for U` needs `Bad`'s head, whose default `::nothing::Nope`
//!   resolves to nothing (the Reference, paths.qualifiers.global-root: a path starting with
//!   `::` names a crate of the extern prelude; `nothing` is none). The impl is a candidate of
//!   `U: Local` and stays unread; the line filed for it (front2/header_written.rs
//!   `head_failed`) names the failed head's item by path and the impl that needed it. Today's
//!   line (the control) is `item ItemKey(<crate prefix>): Resolve(..)`, with no path, no
//!   "needed by", and the impl dropped: the assertions on both paths fail.
//!
//! Prints: `refusal names: deep: <crate>::outer::inner::deep (key ..)`, then
//! `refusal names: local: not checked: .. the head of <crate>::outer::inner::Bad (key ..),
//! needed by the header of <crate>::<impl Local<outer::inner::Bad> for U> (key ..): Resolve(..`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::item_shown::ItemShown;
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

const NESTED: &str = "mod outer { pub mod inner { pub fn deep() {} } }
fn main() { outer::inner::deep() }
";

/// A candidate impl whose header needs a head that fails: `Bad`'s default names nothing.
const NEEDED: &str = "mod outer { pub mod inner { pub struct Bad<T = ::nothing::Nope>(pub T); } }
trait Local<X = u8> { fn l(&self) -> u8 { 0 } }
struct U;
impl Local for U {}
impl Local<outer::inner::Bad> for U {}
fn need_local<T: Local>(_t: T) {}
fn local() { need_local(U) }
fn main() {}
";

#[test]
fn a_nested_item_is_shown_by_its_path() {
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
    let by_lib = ItemShown::of_lib(&names, k).to_string();
    let by_checker = ItemShown::of(&names, &front.keys, checker_of(k)).to_string();
    println!("refusal names: deep: {by_lib}");
    let tail: String = k.0.0[8..14].iter().map(|b| format!("{b:02x}")).collect();
    assert!(by_lib.ends_with(&format!("outer::inner::deep (key {tail})")), "{by_lib}");
    assert_eq!(by_lib, by_checker, "one item, one line, by either key");
}

#[test]
fn a_needed_head_names_its_item_and_the_header_that_needed_it() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), NEEDED.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: NEEDED }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["local"] }, alloc.clone())
        .unwrap_or_else(|e| panic!("`local`: {e}"));
    let k = found.whole(1).unwrap_or_else(|| panic!("`local` not named")).key;
    let outcome = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
        Err(e) => format!("not lowered: {e}"),
        Ok(body) => {
            let view = body.lowered.view();
            let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `local`: {e:?}"));
            let scope = body.scope(&spell);
            let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
            match check_body(&mut records, &body, &scope) {
                Ok(done) if done.diags.is_empty() => "clean".to_string(),
                Ok(done) => {
                    let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                    format!("diags {d:?}")
                }
                Err(e) => format!("not checked: {e}"),
            }
        }
    };
    println!("refusal names: local: {outcome}");
    assert!(outcome.contains("the head of "), "{outcome}");
    assert!(outcome.contains("outer::inner::Bad (key "), "the failed head's item by path: {outcome}");
    assert!(outcome.contains("needed by the header of "), "{outcome}");
    assert!(outcome.contains("<impl Local<outer::inner::Bad> for U> (key "), "the impl that needed it: {outcome}");
    // Only head_failed's naming part: the checker's own gap text around it (`CheckFail`'s
    // Display, the question's Debug) still prints keys and is not this piece's.
    let from = outcome.find("the head of ").expect("asserted above");
    let named = &outcome[from..];
    let named = named.split_once("): ").map_or(named, |(n, _)| n);
    assert!(!named.contains("ItemKey("), "no bare key in head_failed's line: {named}");
}

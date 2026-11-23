//! A REFUSED derive leaves a named gap, never "no impl" (D193 point 5). The Reference,
//! "Derive": "The `derive` attribute allows new items to be automatically generated for data
//! structures", so a derive that is refused wrote impls that are unknown, not absent. The real
//! site is reth's `ChainHardforks` (`#[derive(Default)]`, refused by blaze-expand's item reader
//! before the `element_end` fix): its `T: Default` answered E0277 naming an unrelated blanket
//! impl, a wrong error about a program the pinned nightly builds.
//!
//! The derive input is broken here on purpose: `#[derive(Default)]` on an enum with no
//! `#[default]` variant, which the derive refuses (blaze-expand derive.rs `derive_default`, "no
//! default declared"; the Reference, "Derive", and core/src/default.rs: on an enum the derive
//! needs one variant marked `#[default]`). The refused call's gap row (blaze-scope
//! items_record.rs `each_refused_impl_row`, filed by front2 buckets.rs `file_rows`) is a
//! candidate of every trait question at `E`'s shape, and its header is never made: check.rs
//! `lower_heads` fails it with the refusal's words.
//!
//! - SITE `a_refused_derive_is_a_named_gap`: `need::<E>()` asks `E: Default`; the body is not
//!   checked, and the line names the derive, the item and the derive's reason.
//! - TWIN `an_intact_derive_answers`: the same with `#[default] A`: the derive writes
//!   `impl Default for E`, and the body checks clean.
//! - CONTROL (rule 23, the cause removed): without the gap row (`each_refused_impl_row`
//!   yielding nothing, as before D193 point 5), the site's question has no candidate at `E`'s
//!   shape and the body answers a diagnostic E0277 ("diags [\"E0277 at ..\"]"), the wrong
//!   answer; the site's assertions fail on it.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_refusals_name_items.rs.
//!
//! Prints `refused derive: <case>: <outcome>`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// The site: the derive is refused (no `#[default]` variant).
const REFUSED: &str = "#[derive(Default)]
enum E { A, B }
fn need<T: Default>() {}
fn ask() { need::<E>() }
fn main() {}
";

/// The twin: one variant marked `#[default]`, so the derive writes its impl.
const INTACT: &str = "#[derive(Default)]
enum E { #[default] A, B }
fn need<T: Default>() {}
fn ask() { need::<E>() }
fn main() {}
";

/// The outcome of checking `ask` in a binary whose `src/main.rs` is `text`.
fn check_ask(text: &'static str) -> String {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["ask"] }, alloc.clone())
        .unwrap_or_else(|e| panic!("`ask`: {e}"));
    let k = found.whole(1).unwrap_or_else(|| panic!("`ask` not named")).key;
    match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
        Err(e) => format!("not lowered: {e}"),
        Ok(body) => {
            let view = body.lowered.view();
            let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `ask`: {e:?}"));
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
    }
}

#[test]
fn a_refused_derive_is_a_named_gap() {
    let outcome = check_ask(REFUSED);
    println!("refused derive: site: {outcome}");
    assert!(!outcome.contains("E0277"), "a refused derive's impls are unknown, never absent: {outcome}");
    assert!(outcome.starts_with("not checked: "), "the question is a gap of the body: {outcome}");
    assert!(outcome.contains("unknown: the derive of `Default` on `E`"), "the gap names the derive and its item: {outcome}");
    assert!(outcome.contains("no default declared"), "the gap says why the derive was refused: {outcome}");
}

#[test]
fn an_intact_derive_answers() {
    let outcome = check_ask(INTACT);
    println!("refused derive: twin: {outcome}");
    assert_eq!(outcome, "clean", "the derive writes `impl Default for E`");
}

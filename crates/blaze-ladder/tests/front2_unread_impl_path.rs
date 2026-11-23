//! An impl header whose path does not resolve (census 14's "UnreadImpl Resolve/Path", 20
//! bodies: libc's `impl ::core::default::Default for stat`, written by libc's own macros in the
//! libc the toolchain builds, where `core` is the extern-prelude name of the root's
//! macro-written `cfg_if! { extern crate rustc_std_workspace_core as core; }` under
//! `#![no_core]`). Two fixtures, checked like tests/front2_items.rs (needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain`):
//!
//! - `NOWHERE`: `::nothing` is in no extern prelude, so the impl stays unread and the body
//!   asking its trait's question names it; the line names the header's path as written
//!   (front2/header_written.rs, rule 19). Without that file's wiring the line ends at
//!   "Range { start: .., end: .. })" and the assertion on the written path fails.
//! - `MACRO_ROOT_EXTERN`: the reduced form of libc's case in one crate. The Reference, "Extern
//!   prelude" ([names.preludes.extern.decl]): "If the `extern crate` declaration appears in
//!   the crate root, then the crate name is also added to the extern prelude, so that it is
//!   in scope in all modules"; a macro-expanded one is no exception unless it shadows a name
//!   given with `--extern` (here: none). So `::here` (an `extern crate self as here;` a macro
//!   wrote at the root) names this crate and `impl ::here::Tr for S` is an impl of `Tr`:
//!   `uses` is clean. While front2's path walk (blaze-scope names.rs `walk`, over the crate
//!   root record's `extern_prelude`) does not see a macro-written root `extern crate`, it
//!   prints "not checked: .. unread as written .. the header's path as written:
//!   `::here::Tr`" (the names-core gap sent to the NAMES lane; this test is red until it lands).
//!
//! Prints: `unread path: nowhere: local: not checked: .. the header's path as written:
//! `::nothing::Nope``, then `unread path: macro root extern: uses: clean`.

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

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: its
/// name and its outcome ("clean", the diagnostics, or the failure as printed).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
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
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc.clone())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let outcome = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
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
        got.push((n.to_string(), outcome));
    }
    got
}

/// A global path into no crate: the impl is unread and the gap names its path as written.
const NOWHERE: &str = "trait Local { fn l(&self) -> u8 { 0 } }
struct U;
impl Local for U {}
impl ::nothing::Nope for U {}
fn need_local<T: Local>(_t: T) {}
fn local() { need_local(U) }
fn main() {}
";

/// libc's form, reduced: a macro writes the root's `extern crate`, a header names it globally.
const MACRO_ROOT_EXTERN: &str = "macro_rules! ext { () => { extern crate self as here; } }
ext!();
pub trait Tr { fn t(&self) -> u8 { 3 } }
pub struct S;
impl ::here::Tr for S {}
fn need<T: Tr>(_t: T) {}
fn uses() { need(S) }
fn main() {}
";

#[test]
fn an_unread_header_names_its_path_as_written() {
    let got = check_fns(NOWHERE, &["local"]);
    for (n, o) in &got {
        println!("unread path: nowhere: {n}: {o}");
    }
    assert!(got[0].1.contains("unread as written"), "{got:?}");
    assert!(got[0].1.contains("the header's path as written: `::nothing::Nope`"), "{got:?}");
}

#[test]
fn a_macro_written_root_extern_crate_is_in_the_extern_prelude() {
    let got = check_fns(MACRO_ROOT_EXTERN, &["uses"]);
    for (n, o) in &got {
        println!("unread path: macro root extern: {n}: {o}");
    }
    assert_eq!(got[0].1, "clean", "{got:?}");
}

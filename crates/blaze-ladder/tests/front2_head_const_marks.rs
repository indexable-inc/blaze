//! A HEAD's anonymous constant that a macro wrote is lowered with its tokens' marks (front2/anon.rs
//! `head_marks`, types.rs `const_value_bits`), as a body's is.
//!
//! Rust's rule (the Reference, "Macros By Example", "Hygiene"): a name a macro writes resolves
//! where its token's hygiene says: `$crate` "refers to the crate defining the macro", and a
//! `macro` item is def-site, its names resolving in the module that defines it. The real site:
//! zerocopy 0.8.60's `cryptocorrosion_derive_traits!` writes, into the impl it expands in
//! ppv-lite86, `(): $crate::util::macro_util::PaddingFree<Self, {
//! $crate::cryptocorrosion_derive_traits!(@struct_padding_check #[repr($repr)] ..) }>`. The
//! anonymous constant was filed with no marks, so its `crate` named ppv-lite86 (tempo t1500, 8
//! tests NOT RUN under rand_chacha `refill_wide_impl`: "`const _: usize = crate::
//! cryptocorrosion_derive_traits! ( @ struct_padding_check ..` needs body ..: Macro { path:
//! \"crate:: cryptocorrosion_derive_traits\", why: \"..: segment 0: not found\" }").
//!
//! One crate cannot show `$crate` differ from `crate`, so the fixture uses what does differ in
//! one crate: a def-site `macro` of module `a` (where `K` is 2) writes impls whose constants
//! name `K`, called in module `b` (where `K` is 1). By the rule each impl reads `a::K`; read
//! without the marks, at the impl's module, each reads `b::K` and the wrong impl holds,
//! silently. Shapes: an expression `{ K + 0 }` (an anonymous constant), a plain path `{ K }` (a
//! constant item's value), and `{ <crate::a::S as crate::a::HasN>::N + K }` (a qualified
//! constant leaf answered first and its value written in its place: the marks after it move
//! with the text, anon.rs `head_marks`). Each has the fn that must check clean and its NEGATIVE
//! TWIN (the length the unmarked reading gives, which rustc rejects: E0277). THE CONTROL: the
//! same impl written by hand in `b` reads `b::K` (1).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_head_consts.rs
//! does. Expected lines: `head marks: expr_two: clean`, `head marks: expr_one: diags [..]`,
//! `head marks: path_two: clean`, `head marks: path_one: diags [..]`, `head marks: leaf_five:
//! clean`, `head marks: leaf_four: diags [..]`, `head marks: hand_one: clean`, `head marks:
//! hand_two: diags [..]`. NOT RUN by the writer (a write-only lane, uncompiled).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

const TEXT: &str = "#![feature(decl_macro)]
mod a {
    pub trait Pick<const N: usize> {}
    pub trait HasN { const N: usize; }
    pub struct S;
    pub struct P;
    pub struct R;
    impl HasN for S { const N: usize = 3; }
    pub const K: usize = 2;
    pub macro mk() {
        impl Pick<{ K + 0 }> for S {}
        impl Pick<{ K }> for P {}
        impl Pick<{ <crate::a::S as crate::a::HasN>::N + K }> for R {}
    }
}
mod b {
    pub const K: usize = 1;
    pub struct Q;
    impl crate::a::Pick<{ K + 0 }> for Q {}
    crate::a::mk!();
}
fn need1<T: a::Pick<1>>() {}
fn need2<T: a::Pick<2>>() {}
fn need4<T: a::Pick<4>>() {}
fn need5<T: a::Pick<5>>() {}
fn expr_two() { need2::<a::S>() }
fn expr_one() { need1::<a::S>() }
fn path_two() { need2::<a::P>() }
fn path_one() { need1::<a::P>() }
fn leaf_five() { need5::<a::R>() }
fn leaf_four() { need4::<a::R>() }
fn hand_one() { need1::<b::Q>() }
fn hand_two() { need2::<b::Q>() }
fn main() {}
";

/// Each fn of `fns`, lowered and checked in order over one store, as
/// tests/front2_head_consts.rs `check_fns` does: its name and its outcome ("clean", the
/// diagnostics, or the failure as printed).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let alloc = Alloc::global();
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
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break match check_body(&mut records, &body, &scope) {
                        Ok(done) if done.diags.is_empty() => "clean".to_string(),
                        Ok(done) => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// The def-site constants read `a::K` (2); the hand-written control reads `b::K` (1). Each fn
/// that must hold checks clean, and each twin is a diagnostic of the program: never a gap, and
/// never clean (an impl that read the wrong `K` makes the twin clean and its fn an error).
#[test]
fn a_macro_written_heads_constant_reads_by_its_tokens_marks() {
    let fns = ["expr_two", "expr_one", "path_two", "path_one", "leaf_five", "leaf_four", "hand_one", "hand_two"];
    let clean = [true, false, true, false, true, false, true, false];
    let got = check_fns(TEXT, &fns);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("head marks: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    assert_eq!(got.len(), clean.len(), "{}", lines.join("\n"));
    for ((n, o), want) in got.iter().zip(clean) {
        if want {
            assert_eq!(o, "clean", "`{n}` must check clean\n{}", lines.join("\n"));
        } else {
            assert!(o.starts_with("diags "), "`{n}` must be an error of the program (E0277), not clean and not a gap\n{}", lines.join("\n"));
        }
    }
}

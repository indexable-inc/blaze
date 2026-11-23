//! A fn's generics that need a head not made yet are made, not refused (census 14, brief D;
//! front2/lower_made.rs `lower_made`, the one home of the make-and-lower-again loop).
//!
//! The rule: the Reference, "Trait and lifetime bounds": "Trait and lifetime bounds provide a
//! way for generic items to restrict which types and lifetimes are used as their parameters";
//! and "Paths", generic arguments: a bound may constrain an associated type of its trait
//! (`T<A = u8>`, as the pinned library writes `I: Iterator<Item = u8>`, e.g.
//! library/alloc/src/vec/spec_extend.rs's `I: Iterator<Item = T>`). Reading such a bound needs
//! the trait's head (its generics, its associated types), which is a head like any other: an
//! accepted program does not depend on the order heads were made in. So `fn f<X: T<A = u8>>()`
//! with `T`'s head not made must lower and check clean, and the binding must be the trait's
//! (`X::A` is `u8`), never a hole.
//!
//! Without the fix: front2/body.rs `lower_fn_into` returns `BodyError::Generics { why:
//! Why::Need(<T>) }` and no driver makes `T` (each copied loop only evaluated constants): the
//! census printed `lower Generics: Need <key>` for 21 of tempo's bodies.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_items.rs
//! does. NOT RUN by the writer (a write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// Each fn of `fns` in the crate `text`: first lowered by plain `lower_fn` over a store of its
/// own with nothing made (what today's path says: `generics need` when it stops on
/// `BodyError::Generics { why: Why::Need(_) }`), then by `lower_made` over the one shared
/// store and checked. Its name, the plain outcome and the made outcome ("clean", the
/// diagnostics, or the failure as printed).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String, String)> {
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
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        // Today's path, over a store with nothing made: the face this test is about.
        let mut bare = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
        let plain = match lower_fn(&names, prepared, &mut front.keys, &mut bare, k) {
            Err(BodyError::Generics { why: Why::Need(_) | Why::NeedConst(..), .. })
            | Err(BodyError::Head { error: blaze_ladder::front2::heads::HeadError::Item(_, Why::NeedConst(..)), .. }) => "generics need".to_string(),
            Err(e) => format!("not lowered: {e}"),
            Ok(_) => "lowered".to_string(),
        };
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let made = match lower_made(&mut records, prepared, LowerTarget::Free(k)) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => {
                        let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                        format!("diags {d:?}")
                    }
                    // The program's own errors: an outcome of the check, not a gap.
                    Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                    Err(e) => format!("not checked: {e}"),
                }
            }
        };
        got.push((n.to_string(), plain, made));
    }
    got
}

fn lines(what: &str, got: &[(String, String, String)]) -> String {
    let ls: Vec<String> = got.iter().map(|(n, p, m)| format!("{what}: {n}: plain lower_fn: {p}; lower_made: {m}")).collect();
    for l in &ls {
        println!("{l}");
    }
    ls.join("\n")
}

/// The brief's face: a where-clause (here an inline bound) names a trait whose head is not
/// made, with an associated type binding. Plain `lower_fn` stops on the need (the control:
/// the face is real), and `lower_made` makes the head and lowers clean.
/// Expected line: "unmade trait head: f: plain lower_fn: generics need; lower_made: clean".
#[test]
fn a_bound_on_an_unmade_trait_is_made_then_lowered() {
    const TEXT: &str = "const N: usize = 4;
trait T<const K: usize> { type A; }
fn f<X: T<N, A = u8>>() {}
fn main() {}
";
    let got = check_fns(TEXT, &["f"]);
    let shown = lines("unmade trait head", &got);
    assert_eq!(got.len(), 1, "{shown}");
    assert_eq!(got[0].1, "generics need", "the control: plain `lower_fn` must stop on the generics' need of `N`\n{shown}");
    assert_eq!(got[0].2, "clean", "`f` must lower through `lower_made` and check clean\n{shown}");
}

/// The binding is the made head's, not a hole: `X::A` is `u8` by `T<A = u8>` (the Reference,
/// "Paths in types": `X::A` is `<X as T>::A`, and the bound fixes it), so `one` returning
/// `1u8` is clean and `wide` returning `1u16` is a type mismatch of the program. A fix that
/// lowered the bound with the binding dropped would pass `wide` clean and fail here.
/// Expected lines: "binding kept: one: plain lower_fn: generics need; lower_made: clean" and
/// "binding kept: wide: plain lower_fn: generics need; lower_made: diags [..]".
#[test]
fn the_made_heads_binding_is_kept() {
    const TEXT: &str = "trait T { type A; }
fn one<X: T<A = u8>>() -> X::A { 1u8 }
fn wide<X: T<A = u8>>() -> X::A { 1u16 }
fn main() {}
";
    let got = check_fns(TEXT, &["one", "wide"]);
    let shown = lines("binding kept", &got);
    assert_eq!(got.len(), 2, "{shown}");
    assert_eq!(got[0].2, "clean", "`one` must check clean\n{shown}");
    assert!(got[1].2.starts_with("diags "), "`wide` must be an error of the program, not clean and not a refusal\n{shown}");
}

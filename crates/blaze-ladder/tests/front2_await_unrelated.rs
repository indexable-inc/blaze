//! The value of an `.await` is the future's `Output`, related by what the callee's head says
//! of it; where nothing read says it and the value carries a lifetime, it is made equal to the
//! future's positions, a STRICTER relation (blaze-flow-lower coroutine.rs `await_unrelated`,
//! noted `Form::AwaitValueUnrelated` in `Lowered::coarse`): STRICT RELATIONS REFUSE ONLY ON A
//! FINDING, so a body clean under it stands checked, and one with a finding is refused by that
//! name, the finding dropped (blaze-ladder borrows.rs `check_borrows`).
//!
//! The rules. The Reference, "Await expressions": the value of `f(x).await` is the future's
//! `Output`. For an `async fn` that is the written return type ("Async functions"), which
//! blaze relates (front2_await_output.rs). For `fn f(s: &mut S) -> impl Future<Output = &'static
//! str> + '_`, the `Output` is what the opaque's bound names ("Impl trait": only its bounds are
//! known): here `'static`, so rustc accepts `let v = f(&mut s).await; touch(&mut s); keep(&v);`.
//! blaze reads it from the opaque's `Output = ..` clause (await_call.rs `opaque_output`); before,
//! it refused the body (`NoWrittenReturn`), and before that made the value equal to the
//! opaque's one position, which holds the borrow of `s`: a false E0499.
//!
//! - `rpit_value`: the opaque's `Output = &'static str`: clean;
//! - TWIN `rpit_borrow`: `Output = &u8` borrowing `s` (`+ '_`): E0499, a finding;
//! - `struct_value`: a future STRUCT (`Ready<&'static str>`, its `Output` an impl's `type
//!   Output`): not read yet, so the value is made equal to the future's one position, here
//!   `'static`: no finding under the stricter relation, so CHECKED clean (not refused up front);
//! - `struct_held`: a future struct that HOLDS the borrow of `s` (`Hold<'_, &'static str>`):
//!   rustc accepts (the value is `&'static str`), but under the stricter relation the value takes
//!   the future's borrow of `s` and `touch(&mut s)` conflicts: that finding is the relation's
//!   artifact, so the body is refused by name (`AwaitValueUnrelated`, the callee), no finding;
//! - `async_value`: the same body through an `async fn` with the written `&'static str`: clean;
//! - `rpit_no_position`: an opaque whose `Output` is `u8` (no position, nothing to relate);
//! - `block_value`: an async block of this body awaited as its own future, `async { &v }.await`
//!   (its value related to its captures, the block's own contract): clean.
//!
//! NOT YET RUN: written by a write-only lane (2026-11-21). Every verdict is printed before the
//! assertion.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct S(u8);

pub struct Ready<T>(Option<T>);
impl<T: Unpin> Future for Ready<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> { Poll::Ready(self.0.take().unwrap()) }
}

fn open_rpit(s: &mut S) -> impl Future<Output = &'static str> + '_ { async move { let _ = s; "x" } }
fn open_borrow(s: &mut S) -> impl Future<Output = &u8> + '_ { async move { &s.0 } }
fn ready_of(_s: &mut S) -> Ready<&'static str> { Ready(Some("x")) }

pub struct Hold<'a, T>(&'a mut S, Option<T>);
impl<'a, T: Unpin> Future for Hold<'a, T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> { Poll::Ready(self.1.take().unwrap()) }
}
fn hold(s: &mut S) -> Hold<'_, &'static str> { Hold(s, Some("x")) }
async fn open_async(_s: &mut S) -> &'static str { "x" }
fn count(s: &mut S) -> impl Future<Output = u8> + '_ { async move { s.0 } }
fn touch(_: &mut S) {}
fn keep<T>(_: &T) {}

async fn rpit_value(mut s: S) { let v = open_rpit(&mut s).await; touch(&mut s); keep(&v); }
async fn rpit_borrow(mut s: S) { let v = open_borrow(&mut s).await; touch(&mut s); keep(&v); }
async fn struct_value(mut s: S) { let v = ready_of(&mut s).await; touch(&mut s); keep(&v); }
async fn struct_held(mut s: S) { let v = hold(&mut s).await; touch(&mut s); keep(&v); }
async fn async_value(mut s: S) { let v = open_async(&mut s).await; touch(&mut s); keep(&v); }
async fn rpit_no_position(mut s: S) { let v = count(&mut s).await; touch(&mut s); keep(&v); }
async fn block_value() -> u8 { let v = 1u8; let r = async { &v }.await; *r }

fn main() {}
"#;

/// Each body's verdict as text: `clean`, `findings N`, or `refused <the refusal>`, each printed.
fn verdicts() -> Vec<(&'static str, String)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/awu/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "awu",
        dir: "/awu/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for name in ["rpit_value", "async_value", "rpit_no_position", "block_value", "rpit_borrow", "struct_value", "struct_held"] {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        println!("{name}: types clean {}, {v:?}", types.is_clean());
        assert!(types.is_clean(), "`{name}`'s types: {types:?}");
        let got = match &v.refused {
            Some(r) => format!("refused {r:?}"),
            None if v.findings.is_empty() => "clean".to_owned(),
            None => format!("findings {}", v.findings.len()),
        };
        out.push((name, got));
    }
    out
}

#[test]
fn an_awaited_value_is_its_futures_output_or_refused_by_name() {
    let got = verdicts();
    println!("verdicts: {got:?}");
    assert_eq!(got[0], ("rpit_value", "clean".to_owned()), "the opaque's `Output = &'static str` holds no borrow of `s`");
    assert_eq!(got[1], ("async_value", "clean".to_owned()), "the written return type relates it");
    assert_eq!(got[2], ("rpit_no_position", "clean".to_owned()), "a value with no position needs nothing");
    assert_eq!(got[3], ("block_value", "clean".to_owned()), "an async block's value is related to its captures");
    assert!(got[4].0 == "rpit_borrow" && got[4].1.starts_with("findings"), "TWIN: `Output = &u8` borrowing `s` is E0499: {:?}", got[4]);
    assert_eq!(got[5], ("struct_value", "clean".to_owned()), "clean under the stricter relation: checked, not refused up front");
    assert!(
        got[6].0 == "struct_held" && got[6].1.starts_with("refused") && got[6].1.contains("AwaitValueUnrelated {") && got[6].1.contains("callee: Some("),
        "a finding under the stricter relation is refused by its name, never reported: {:?}",
        got[6]
    );
}

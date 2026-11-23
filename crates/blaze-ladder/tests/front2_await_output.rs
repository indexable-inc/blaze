//! The value of `f(&mut x).await` for an `async fn f` holds only what `f`'s WRITTEN return
//! type names, never the borrow the future captured (blaze-flow-lower await_call.rs
//! `awaited_shadow`, coroutine.rs `await_done`).
//!
//! The rules. The Reference, "Async functions": `async fn f<'a, P>(s: &'a mut S<P>) -> W<P>`
//! is "roughly equivalent to" `fn f<'a, P>(s: &'a mut S<P>) -> impl Future<Output = W<P>> +
//! 'a`: the future holds the borrow of `s` until it is done; its output is the written `W<P>`.
//! "Await expressions": the value of `f(&mut s).await` is the future's `Output`. So after
//! `let w = f(&mut s).await;` the borrow of `s` is over and `touch(&mut s)` while `w` is held
//! is accepted (rustc accepts every clean body below). TWINS: a written return type that
//! names the argument's lifetime (`async fn get<P>(s: &mut S<P>) -> &P`) keeps the borrow in
//! the value: `touch(&mut s)` before the value's last use is E0499.
//!
//! The bodies: the signature path (no type argument with a lifetime, `peek_static`), the
//! generic path (`P = &'static str`), and tempo's shape (tip20_channel_reserve.rs:229,
//! `ChannelEnv::open(&mut payer, ..).await` with `payer: TempoTxSender<impl Provider>`): an
//! associated `async fn` returning `Result<Self, _>` of an impl over `P`, `P` an opaque type.
//! Before the fix every clean body here had a borrow finding (the poll's result was made equal
//! to the future's one opaque position, which holds the borrow of `s`). NOT YET RUN: written
//! by a write-only lane (2026-11-20). Every verdict is printed before the assertion.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub struct S<P>(P);
pub struct W<P>(P);
pub struct Env<P> { p: P }

impl<P: Clone> Env<P> {
    async fn open(s: &mut S<P>) -> Result<Self, ()> { Ok(Env { p: s.0.clone() }) }
}

async fn peek_static(_s: &mut S<u8>) -> &'static str { "x" }
async fn peek(s: &mut S<u8>) -> &u8 { &s.0 }
async fn open<P: Clone>(s: &mut S<P>) -> W<P> { W(s.0.clone()) }
async fn get<P>(s: &mut S<P>) -> &P { &s.0 }
fn touch<P>(_: &mut S<P>) {}
fn keep<T>(_: &T) {}
fn provider() -> impl Clone { 7u8 }

async fn by_signature(mut s: S<u8>) { let v = peek_static(&mut s).await; touch(&mut s); keep(&v); }
async fn by_signature_twin(mut s: S<u8>) { let v = peek(&mut s).await; touch(&mut s); keep(&v); }
async fn by_generics(mut s: S<&'static str>) { let w = open(&mut s).await; touch(&mut s); keep(&w); }
async fn by_generics_twin(mut s: S<&'static str>) { let w = get(&mut s).await; touch(&mut s); keep(&w); }
async fn tempo_shape() { let mut s = S(provider()); let env = Env::open(&mut s).await; touch(&mut s); keep(&env); }
async fn tempo_shape_twin() { let mut s = S(provider()); let p = get(&mut s).await; touch(&mut s); keep(&p); }

fn main() {}
"#;

/// Each body's verdict: `Some(true)` a borrow finding, `Some(false)` clean, `None` refused
/// (not checked), each printed.
fn verdicts() -> Vec<(&'static str, Option<bool>)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/awo/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "awo", dir: "/awo/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for name in ["by_signature", "by_signature_twin", "by_generics", "by_generics_twin", "tempo_shape", "tempo_shape_twin"] {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        println!("{name}: types clean {}, {v:?}", types.is_clean());
        assert!(types.is_clean(), "`{name}`'s types: {types:?}");
        let got = if v.refused.is_some() { None } else { Some(!v.findings.is_empty()) };
        out.push((name, got));
    }
    out
}

#[test]
fn an_awaited_value_holds_only_its_written_return_types_regions() {
    let got = verdicts();
    println!("verdicts: {got:?}");
    let want = [
        ("by_signature", Some(false)),
        ("by_signature_twin", Some(true)),
        ("by_generics", Some(false)),
        ("by_generics_twin", Some(true)),
        ("tempo_shape", Some(false)),
        ("tempo_shape_twin", Some(true)),
    ];
    assert_eq!(got, want, "an awaited value borrows only what its written return type names");
}

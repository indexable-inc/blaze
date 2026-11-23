//! Async call syntax and parenthesized bounds through the production front end and core.
//! Each positive body uses its awaited result at the declared type. The negative body
//! returns a bool where the bound requires the future's output to be u8.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::reads::FactKind;
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use core::ops::{AsyncFn, AsyncFnMut, AsyncFnOnce};

struct Input { byte: u8 }

async fn once(f: impl AsyncFnOnce(&str) -> usize, s: &str) -> usize { f(s).await }
async fn mutable(mut f: impl AsyncFnMut(Input) -> u8) -> u8 { f(Input { byte: 1 }).await }
async fn shared(f: impl AsyncFn(Input) -> u8) -> u8 { f(Input { byte: 2 }).await }
fn missing_await(f: impl AsyncFnOnce() -> u8) -> u8 { f() }
trait Other<T> { type Output; }
impl<F, T> Other<T> for F { type Output = bool; }
fn two_bounds<F: Other<u16, Output = bool> + AsyncFnOnce(Input) -> u8>(_f: F) {}

async fn typed(s: &str) -> usize { once(async |x: &str| x.len(), s).await }
async fn inferred() -> u8 { shared(async |x| x.byte).await }
async fn returning_future() -> u8 { shared(|x| async move { x.byte }).await }
async fn writes() -> u8 {
    let mut seen = 0u8;
    mutable(async |x| { seen = x.byte; seen }).await
}
async fn wrong_output() -> u8 { shared(async |_x| false).await }
fn other_projection() { two_bounds(async |i| i.byte); }
fn consume_once(owned: Input) { two_bounds(async move |i| { owned; i.byte }); }
async fn needs_shared(owned: Input) -> u8 { shared(async move |i| { owned; i.byte }).await }

fn main() {}
"#;

#[test]
fn async_callables_use_their_argument_tuple_and_awaited_output() {
    check_bodies(&["typed", "inferred", "returning_future", "writes", "other_projection", "consume_once", "wrong_output", "needs_shared"]);
}

#[test]
fn generic_async_calls_return_the_call_future_for_the_receiver() {
    check_bodies(&["once", "mutable", "shared", "missing_await"]);
}

fn check_bodies(cases: &[&str]) {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let files = [("/callables/src/main.rs".to_string(), SOURCE.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "callables", dir: "/callables/", root: "/callables/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).expect("user crate and library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve crates");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("command frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("production body checker");
    let bodies = world.bodies(alloc);
    for &name in cases {
        let (body, item) = bodies.iter().find_map(|(b, _)| {
            let BodyRef::Free(k) = *b else { return None };
            let item = names.item_text(k)?;
            (item.name == Some(name)).then_some((*b, item.text))
        }).unwrap_or_else(|| panic!("missing body {name}"));
        let types = world.check(body, alloc);
        assert!(types.refused.is_none() && types.gaps.is_empty() && types.undecided.is_empty(), "{name}: {types:?}");
        assert!(types.reads.is_some() && types.findings.is_empty(), "{name}: {types:?}");
        if let Some(method) = match name {
            "once" | "missing_await" => Some("async_call_once"),
            "mutable" => Some("async_call_mut"),
            "shared" => Some("async_call"),
            _ => None,
        } {
            let facts = types.facts.as_ref().expect("completed check's read list");
            assert!(facts.rows.iter().any(|r| {
                r.fact.kind == FactKind::AssocItem && r.fact.detail == blaze_typeck::Name::of(method).0 && r.answer.is_some()
            }), "{name} must select {method}: {facts:?}");
        }
        if matches!(name, "wrong_output" | "missing_await") {
            let [error] = &types.diagnostics[..] else { panic!("{name}: expected one mismatch: {types:?}") };
            assert_eq!(error.code, "E0308", "{types:?}");
            let expr = if name == "missing_await" { "f()" } else { "false" };
            assert_eq!(item.get(error.start as usize..error.end as usize), Some(expr), "{types:?}");
        } else if name == "needs_shared" {
            assert!(types.diagnostics.iter().any(|d| d.code == "E0277" && d.text.contains("AsyncFn")), "{name}: {types:?}");
        } else {
            assert!(types.diagnostics.is_empty(), "{name}: {types:?}");
        }
    }
}

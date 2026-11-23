//! A return-position `impl Future` is checked against the opaque signature lifetime even though
//! its body expression has the concrete `PNode::Future` type and positions for its captures. The
//! fixture is edition 2024, so every in-scope input lifetime is captured and must outlive the
//! opaque output region. A future carrying a reference to a local still reports E0515.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;

struct Pair<'p>(&'p u8, &'p u8);
struct Pair2<'a, 'b>(&'a u8, &'b u8);

fn clean(first: &u8, second: &u8) -> impl Future<Output = u8> {
    async move { *first + *second }
}

fn no_capture() -> impl Future<Output = ()> {
    async {}
}

fn uncaptured_local() -> impl Future<Output = u8> {
    let local = 1u8;
    let reference = &local;
    async move { *reference }
}

fn wrong() -> impl Future<Output = u8> {
    async { false }
}

fn f<'a>(x: &'a u8) -> (&'a u8, &'a u8) {
    (x, x)
}

fn g(x: &u8) -> Pair<'_> {
    Pair(x, x)
}

fn two_lifetimes<'a, 'b>(first: &'a u8, second: &'b u8) -> Pair2<'a, 'b> {
    Pair2(first, second)
}

async fn async_borrowed<'a>(x: &'a u8) -> &'a u8 {
    x
}

fn main() {}
"#;

#[test]
fn impl_future_body_uses_the_signature_output_region() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let toolchain = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("toolchain");
    let root = "/w2/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("edition");
    let user = UserCrate { name: "w2", dir: "/w2/", root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &toolchain).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front end");
    let bodies = world.bodies(alloc);
    let body = |name: &str| {
        bodies.iter().find_map(|&(body, _)| match body {
            BodyRef::Free(key) if names.item_text(key)?.name == Some(name) => Some(body),
            _ => None,
        }).unwrap_or_else(|| panic!("missing body {name}"))
    };

    for name in ["clean", "no_capture"] {
        let (types, borrows) = world.check_borrows(body(name), alloc);
        assert!(types.is_clean() && types.findings.is_empty() && types.undecided.is_empty(), "{name}: {types:?}");
        assert!(borrows.is_clean(), "{name}: {borrows:?}");
        println!("{name}: clean");
    }

    let (types, borrows) = world.check_borrows(body("uncaptured_local"), alloc);
    assert!(types.is_clean() && types.findings.is_empty() && types.undecided.is_empty(), "uncaptured_local: {types:?}");
    assert!(borrows.refused.is_none(), "uncaptured_local: {borrows:?}");
    assert!(borrows.findings.iter().any(|finding| finding.code == "E0515"), "uncaptured_local: {borrows:?}");
    println!("uncaptured_local: E0515");

    for name in ["f", "g", "two_lifetimes", "async_borrowed"] {
        let (types, borrows) = world.check_borrows(body(name), alloc);
        if !types.is_clean() {
            println!("{name}: type verdict {types:?}");
            continue;
        }
        if borrows.is_clean() {
            println!("{name}: clean");
        } else if let Some(refused) = borrows.refused {
            println!("{name}: refused {refused:?}");
        } else {
            println!("{name}: findings {:?}", borrows.findings);
        }
    }

    let types = world.check(body("wrong"), alloc);
    assert!(!types.diagnostics.is_empty(), "wrong: the async output bool must not satisfy `Future<Output = u8>`");
    println!("wrong: type error");
}

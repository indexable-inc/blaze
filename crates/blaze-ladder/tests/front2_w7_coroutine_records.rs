//! Check the caller first. Its Send obligation reads the defining coroutine
//! record even when the pending future was made by another function.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::reads::FactKind;
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
fn send<T: Send>(_: T) {}
fn request(value: Arc<Vec<u8>>) -> impl Future<Output = usize> {
    async move { value.len() }
}
async fn relay(value: Arc<Vec<u8>>) -> usize { request(value).await }
fn local_request(value: Rc<Vec<u8>>) -> impl Future<Output = usize> {
    async move { value.len() }
}
fn local_work() -> impl Future<Output = usize> {
    let value = Rc::new(Vec::<u8>::new());
    local_request(value)
}
async fn pending_only() -> usize { local_work().await }
fn caller(value: Arc<Vec<u8>>) { send(relay(value)); }
fn local_caller(value: Rc<Vec<u8>>) { send(local_request(value)); }
fn local_pending() { send(pending_only()); }
fn main() {}
"#;

#[test]
fn callers_read_the_captured_and_pending_types_from_the_producer() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/relay/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "relay", dir: "/relay/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key = |name: &str| {
        names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name)
    };
    let caller = world.check(BodyRef::Free(key("caller")), alloc);
    assert!(caller.is_clean() && caller.undecided.is_empty() && caller.reads.is_some(), "{caller:?}");
    let facts = &caller.facts.as_ref().expect("caller read list").rows;
    for producer in ["relay", "request"] {
        let owner = checker_of(key(producer));
        assert!(facts.iter().any(|r| r.fact.kind == FactKind::Coroutine && r.fact.key == owner.0 && r.answer.is_some()),
            "missing coroutine read for {producer}: {facts:?}");
    }
    println!("caller: clean (relay and request coroutine records)");

    for name in ["local_caller", "local_pending"] {
        let control = world.check(BodyRef::Free(key(name)), alloc);
        assert!(control.refused.is_none() && control.gaps.is_empty() && control.undecided.is_empty(), "{name}: {control:?}");
        let [d] = control.diagnostics.as_slice() else { panic!("{name}: expected one Send error: {control:?}") };
        assert_eq!(d.code, "E0277", "{d:?}");
        assert!(d.text.contains("Send"), "{d:?}");
        println!("{name}: E0277 (Send)");
    }
}

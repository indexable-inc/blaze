//! A state hook owns its smart pointer even when its receiver autoderefs.
//! A factory constructing an async move block transfers its captured value again.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::rc::Rc;
use std::sync::Arc;
fn send<T: Send>(_: T) {}
fn call_twice<F: Fn() -> R, R>(_: F) {}
fn hook(value: Arc<Vec<u8>>) { send(move || value.len()); }
fn borrowed_hook(value: Arc<Vec<u8>>) { send(|| value.len()); }
fn local_hook(value: Rc<Vec<u8>>) { send(move || value.len()); }
fn once<F: FnOnce() + Send>(f: F) { send(|| f()); }
fn repeat<F: FnMut() + Send>(mut f: F) { send(|| f()); }
fn factory(value: String) { send(move || async move { value.len() }); }
fn local_factory(value: Rc<Vec<u8>>) { send(move || async move { value.len() }); }
fn discard_place(value: Rc<u8>) { send(move || { let _ = value; }); }
fn copied_target(value: Box<u8>) { call_twice(move || *value); }
fn repeatable_factory(value: u8) { call_twice(move || async move { value }); }
fn consumed_factory(value: String) { call_twice(move || async move { value.len() }); }
fn consume_once(value: String) {
    let consume = async move || drop(value);
    send(consume());
}
fn consume_local(value: Rc<u8>) {
    let consume = async move || drop(value);
    send(consume());
}
fn main() {}
"#;

#[test]
fn hooks_and_future_factories_use_their_capture_modes() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/w7/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "w7", dir: "/w7/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for (name, send) in [
        ("hook", true), ("borrowed_hook", true), ("local_hook", false), ("once", true), ("repeat", true),
        ("factory", true), ("local_factory", false), ("discard_place", true),
        ("consume_once", true), ("consume_local", false), ("copied_target", true), ("repeatable_factory", true),
    ] {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let v = world.check(BodyRef::Free(key), alloc);
        assert!(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none(), "{name}: {v:?}");
        if send {
            assert!(v.is_clean() && v.reads.is_some(), "{name}: {v:?}");
            println!("{name}: clean");
        } else {
            let [d] = v.diagnostics.as_slice() else { panic!("{name}: expected one Send error: {v:?}") };
            assert_eq!(d.code, "E0277", "{name}: {d:?}");
            assert!(d.text.contains("Send"), "{name}: {d:?}");
            println!("{name}: E0277 (Send)");
        }
    }
    let key = names.items(0).find_map(|(key, _, _)| {
        (names.item_text(key)?.name == Some("consumed_factory")).then_some(key)
    }).expect("consumed_factory");
    let v = world.check(BodyRef::Free(key), alloc);
    assert!(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none(), "{v:?}");
    let [d] = v.diagnostics.as_slice() else { panic!("expected one Fn error: {v:?}") };
    assert_eq!(d.code, "E0277", "{d:?}");
    assert!(d.text.contains("Fn"), "{d:?}");
    println!("consumed_factory: E0277 (Fn)");
}

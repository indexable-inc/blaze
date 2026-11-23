//! A closure stores its captures. A suspended async block also stores the locals
//! and pending future needed to resume at its await.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::rc::Rc;
use std::cell::Cell;
fn need<T: Send>(_: T) {}
struct Wrapped<T>(T);
fn captures(number: u32, text: &'static str) {
    need(move || (number, text));
}
fn captures_rc(value: Rc<u8>) {
    need(Wrapped(move || drop(value)));
}
fn only_returns_rc() {
    need(|| Rc::new(1u8));
}
fn borrows_cell(value: Cell<u8>) {
    need(|| drop(&value));
}
fn owns_cell(value: Cell<u8>) {
    need(move || drop(&value));
}
fn holds_rc() {
    need(async {
        let value = Rc::new(1u8);
        async {}.await;
        drop(value);
    });
}
fn holds_byte() {
    need(async {
        let value = 1u8;
        async {}.await;
        drop(value);
    });
}
trait Make { type Output; }
impl<F: FnOnce() -> T, T> Make for F { type Output = T; }
struct Buffered<F: Make> { f: F, pending: Option<F::Output> }
fn buffered<F: Make>(f: F) -> Buffered<F> { Buffered { f, pending: None } }
fn stores_closure_output() {
    need(buffered(|| async {
        drop(Rc::new(1u8));
        async {}.await;
    }));
}
fn stores_non_send_output() {
    need(buffered(|| async {
        let kept = Rc::new(1u8);
        async {}.await;
        drop(kept);
    }));
}
fn make<T>(value: T) -> impl FnOnce() -> T { move || value }
fn returned_byte() { need(make(1u8)); }
fn returned_rc() { need(make(Rc::new(1u8))); }
fn main() {}
"#;

#[test]
fn send_depends_on_captures_and_suspended_state() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/eb6/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "eb6", dir: "/eb6/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture and standard library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for (name, send) in [
        ("captures", true), ("captures_rc", false), ("only_returns_rc", true),
        ("borrows_cell", false), ("owns_cell", true), ("holds_rc", false),
        ("holds_byte", true), ("stores_closure_output", true), ("stores_non_send_output", false),
        ("returned_byte", true), ("returned_rc", false),
    ] {
        let key = names.items(0).find_map(|(key, _, _)| {
            (names.item_text(key)?.name == Some(name)).then_some(key)
        }).expect("fixture body");
        let (v, _) = world.check_borrows(BodyRef::Free(key), alloc);
        assert!(v.refused.is_none() && v.gaps.is_empty() && v.undecided.is_empty(), "{name}: {v:?}");
        if send {
            assert!(v.is_clean() && v.reads.is_some(), "{name}: {v:?}");
            println!("{name}: clean");
        } else {
            let [d] = &v.diagnostics[..] else { panic!("{name}: expected one Send error: {v:?}") };
            assert_eq!(d.code, "E0277", "{name}: {d:?}");
            assert!(d.text.contains("Send"), "{name}: {d:?}");
            println!("{name}: E0277 (Send)");
        }
    }
}

//! A caller of another fn's `impl Trait` or `async fn` sees its declared bounds, and its
//! auto traits through the hidden type ("Auto trait leakage"): a `Send` asked of the returned
//! future is answered by the defining body's stored state, read as a record of that body.
//! A body using only the declared bounds asks for no hidden type; a body needing `Send` of
//! the result is clean when every stored value is `Send`, and E0277 when an `Rc` is held
//! across an await (the control).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;
fn require_send<T: Send>(_: T) {}
fn require_output<F: Future<Output = u8>>(_: F) {}
async fn ready() {}

fn made() -> impl Future<Output = u8> {
    async { 1u8 }
}
// (a) only the declared bound `Future<Output = u8>`: no auto trait, no hidden type.
fn declared_bounds_only() { require_output(made()); }

async fn sendable() -> u8 {
    let kept = 2u8;
    ready().await;
    kept
}
fn rpit_block() -> impl Future<Output = ()> {
    async {
        let kept = 3u8;
        ready().await;
        drop(kept);
    }
}
// (b) `Send` of the result, leaked through the hidden type: every stored value is `Send`.
fn needs_send_async_fn() { require_send(sendable()); }
fn needs_send_rpit() { require_send(rpit_block()); }
// The `Rc` is dropped before the await: it is not stored, so the future is `Send`.
async fn dropped_first() {
    let r = Rc::new(1u8);
    drop(r);
    ready().await;
}
fn needs_send_after_drop() { require_send(dropped_first()); }

// Control: the `Rc` is held across the await, so the future is not `Send`.
async fn holds_rc() {
    let r = Rc::new(1u8);
    ready().await;
    drop(r);
}
fn control_rc_across_await() { require_send(holds_rc()); }

// Receiver evaluation precedes argument evaluation. Moving Cell<u8> is sound, but
// moving the shared receiver borrow of Cell<u8> across threads would require Sync.
async fn borrow_during_argument(cell: Cell<u8>) {
    cell.set(async { 1u8 }.await);
}
async fn borrow_after_argument(cell: Cell<u8>) {
    let value = async { 1u8 }.await;
    cell.set(value);
}
fn pending_receiver(cell: Cell<u8>) { require_send(borrow_during_argument(cell)); }
fn later_receiver(cell: Cell<u8>) { require_send(borrow_after_argument(cell)); }
fn main() {}
"#;

#[test]
fn hidden_types_answer_auto_traits_and_only_them() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/w6/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "w6", dir: "/w6/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture and standard library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for (name, clean) in [
        ("declared_bounds_only", true),
        ("needs_send_async_fn", true),
        ("needs_send_rpit", true),
        ("needs_send_after_drop", true),
        ("control_rc_across_await", false),
        ("pending_receiver", false),
        ("later_receiver", true),
    ] {
        let key = names.items(0).find_map(|(key, _, _)| {
            (names.item_text(key)?.name == Some(name)).then_some(key)
        }).expect("fixture body");
        let v = world.check(BodyRef::Free(key), alloc);
        assert!(v.refused.is_none() && v.gaps.is_empty() && v.undecided.is_empty(), "{name}: {v:?}");
        assert!(v.reads.is_some(), "{name}: a finished check has a read list: {v:?}");
        if clean {
            assert!(v.is_clean(), "{name}: {v:?}");
            println!("{name}: clean");
        } else {
            let [d] = &v.diagnostics[..] else { panic!("{name}: expected one Send error: {v:?}") };
            assert_eq!(d.code, "E0277", "{name}: {d:?}");
            assert!(d.text.contains("Send"), "{name}: {d:?}");
            println!("{name}: E0277 (Send)");
        }
    }
}

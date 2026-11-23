//! A closure that captures an async block's future, boxed to `Box<dyn FnMut() -> bool + Send>`:
//! its `Send` is its captures' (the Reference, "Closure types": "A closure is `Send` if all of
//! its captures are `Send`"), and the future's is its interior's, decided on what it holds
//! across an `.await` ("Async blocks"; rustc decides it once the body's coroutine witnesses are
//! known). blaze-typeck rules/coerce_interior.rs `holds_fixed_interior` decides whether a
//! failed or undecided `CoerceUnsized` may rest only on a future's interior. It did not look
//! into a closure's environment: `ty_children` walks a closure's signature, not its captures.
//!
//! turso sync/engine/src/database_sync_lazy_storage.rs:589 (check.out c1851p, E0283 "cannot
//! satisfy `Box<{closure #6 of read_page}>: CoerceUnsized<Box<dyn FnMut<(), Output = bool> +
//! Send>>` ... what is held by the async block `async { .. }` is not decided yet"):
//! `self.sync_engine_io.add_io_callback(Box::new(move || match generator.resume_with(Ok(()))
//! { .. }))`. `generator` is a genawaiter `sync::Gen` made from `move |coro| async move { ..
//! }`, which holds the block's future as `Pin<Box<F>>`. Reduced, with no genawaiter:
//!
//! - ORACLE `sends`: the block holds a `String` and a `u8` across its `.await`. It is `Send`,
//!   so the body is clean.
//! - TWIN `holds_rc`: the block holds an `Rc<u8>` across its `.await`. That is not `Send`,
//!   which rustc reports as E0277 ("future cannot be sent between threads safely").
//! - CONTROL `direct` (rule 23: the closure layer removed): the same `Gen` boxed as `Box<dyn
//!   Resume + Send>`. The future is reached through the ADT's arguments, which the walk always
//!   saw, so the body is clean with or without the fix.
//!
//! NOT YET RUN: written by a write-only lane (2026-11-21). Every verdict is printed before
//! its assertion.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Waker};

pub struct Gen<F> {
    fut: Pin<Box<F>>,
}
impl<F: Future<Output = ()>> Gen<F> {
    pub fn new<P: FnOnce(u8) -> F>(producer: P) -> Self {
        Gen { fut: Box::pin(producer(0)) }
    }
    pub fn resume(&mut self) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        self.fut.as_mut().poll(&mut cx).is_ready()
    }
}
pub trait Resume {
    fn step(&mut self) -> bool;
}
impl<F: Future<Output = ()>> Resume for Gen<F> {
    fn step(&mut self) -> bool {
        self.resume()
    }
}
pub fn add(_cb: Box<dyn FnMut() -> bool + Send>) {}
pub fn add_gen(_g: Box<dyn Resume + Send>) {}
async fn wait(_: u8) {}

pub fn sends(name: String) {
    let mut generator = Gen::new(move |n| async move {
        wait(n).await;
        drop(name);
    });
    add(Box::new(move || generator.resume()));
}
pub fn holds_rc() {
    let mut generator = Gen::new(move |n| async move {
        let r = std::rc::Rc::new(n);
        wait(n).await;
        drop(r);
    });
    add(Box::new(move || generator.resume()));
}
pub fn direct(name: String) {
    let generator = Gen::new(move |n| async move {
        wait(n).await;
        drop(name);
    });
    add_gen(Box::new(generator));
}
fn main() {}
"#;

/// Checks each of `bodies` of the one-crate program, handing each verdict to `see`.
fn check_each(bodies: &[&str], see: &mut dyn FnMut(&str, &BodyVerdict<'_>)) {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/chf/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "chf", dir: "/chf/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for &name in bodies {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let v = world.check(BodyRef::Free(key), alloc);
        println!("{name}: {v:?}");
        see(name, &v);
    }
}

#[test]
fn a_closure_holding_a_send_future_boxes_to_a_send_object() {
    check_each(&["sends"], &mut |name, v| {
        assert!(v.is_clean() && v.reads.is_some(), "ORACLE `{name}` must be clean (its future is `Send` on what it holds): {v:?}");
    });
}

#[test]
fn a_closure_holding_a_future_with_an_rc_across_await_is_not_send() {
    check_each(&["holds_rc"], &mut |name, v| {
        assert!(v.gaps.is_empty() && v.refused.is_none(), "TWIN `{name}`: a gap or a refusal, not the finding: {v:?}");
        assert!(
            !v.diagnostics.is_empty() && v.diagnostics.iter().all(|d| d.code == "E0277"),
            "TWIN `{name}` must be E0277 (`Rc<u8>` held across the `.await`), never E0283: {v:?}"
        );
    });
}

#[test]
fn the_future_reached_through_adt_arguments_was_always_seen() {
    check_each(&["direct"], &mut |name, v| {
        assert!(v.is_clean() && v.reads.is_some(), "CONTROL `{name}` must be clean: {v:?}");
    });
}

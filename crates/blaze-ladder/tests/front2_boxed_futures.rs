//! Check boxed return values through the production front end, including the library impls
//! of `CoerceUnsized` for `Pin` and `Box`. The stream trait and extension method reproduce
//! the associated-type signature of `StreamExt::boxed` without a dependency on futures-util.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const PROGRAM: &str = r#"
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn immediate(value: &u8) -> BoxFuture<'_, u8> {
    Box::pin(async move { *value })
}

fn suspending(value: &u8) -> BoxFuture<'_, Result<u8, ()>> {
    Box::pin(async move {
        if *value == 0 { return Err(()); }
        let temporary = Rc::new(*value);
        drop(temporary);
        async {}.await;
        Ok(*value)
    })
}

fn different_output(value: BoxFuture<'_, u8>) -> BoxFuture<'_, bool> { value }

trait Task: Future + Send {}
fn require_send<T: ?Sized + Send>(_value: &T) {}
fn listed(value: &(dyn Future<Output = u8> + Send)) { require_send(value); }
fn inherited(value: &dyn Task<Output = u8>) { require_send(value); }
fn absent(value: &dyn Future<Output = u8>) { require_send(value); }

trait Stream { type Item; }
type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;
trait StreamExt: Stream {
    fn boxed<'a>(self) -> BoxStream<'a, Self::Item> where Self: Sized + Send + 'a {
        Box::pin(self)
    }
}
impl<S: Stream> StreamExt for S {}
struct Bytes;
impl Stream for Bytes { type Item = u8; }

fn boxed_stream(stream: Bytes) -> BoxStream<'static, u8> { stream.boxed() }
fn different_item(value: BoxStream<'static, u8>) -> BoxStream<'static, bool> { value }
fn main() {}
"#;

/// A ground projection whose impl's type argument is the future of this very body.
/// Changing the block's output changes the impl's associated type.
const LOCAL_FUTURE: &str = r#"
use std::future::Future;
struct Job<F>(F);
trait JobOutput { type Value; }
impl<F: Future> JobOutput for Job<F> { type Value = F::Output; }
fn output<J: JobOutput>(_: J) -> J::Value { loop {} }
fn byte() -> u8 { output(Job(async { 1u8 })) }
fn wrong() -> u8 { output(Job(async { false })) }
fn main() {}
"#;

/// While `outer` is suspended it owns the future of the inner block. Moving an
/// `Rc` into that block changes the outer opaque's auto trait at its caller.
const NESTED_FUTURE: &str = r#"
use std::future::Future;
use std::rc::Rc;
fn outer() -> impl Future<Output = ()> { async { async {}.await } }
fn held(r: Rc<u8>) -> impl Future<Output = ()> {
    async move { async move { drop(r) }.await }
}
fn require<T: Send>(_: T) {}
fn send() { require(outer()); }
fn wrong(r: Rc<u8>) { require(held(r)); }
fn main() {}
"#;

fn check_future_imports(program: &str, cases: &[(&str, Option<&str>)]) {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let toolchain = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let files = [("/imports/src/main.rs".to_owned(), program.to_owned())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
    let user = UserCrate { name: "imports", dir: "/imports/", root: "/imports/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &toolchain).expect("fixture and toolchain");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve the fixture");
    let sources = [SourceFile { path: "src/main.rs", text: program }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("inference frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front end");
    let bodies = world.bodies(alloc);
    for &(name, error) in cases {
        let b = bodies.iter().find_map(|&(b, _)| match b {
            BodyRef::Free(k) if names.item_text(k)?.name == Some(name) => Some(b),
            _ => None,
        }).unwrap_or_else(|| panic!("missing {name}"));
        let got = world.check(b, alloc);
        assert!(got.gaps.is_empty() && got.refused.is_none() && got.undecided.is_empty(), "{name}: {got:?}");
        let codes: Vec<_> = got.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes.as_slice(), error.as_slice(), "{name}: {got:?}");
        if error.is_none() {
            assert!(got.is_clean() && got.reads.is_some() && got.findings.is_empty(), "{name}: {got:?}");
        }
    }
}

#[test]
fn a_local_future_is_an_instance_argument_during_writeback() {
    check_future_imports(LOCAL_FUTURE, &[("byte", None), ("wrong", Some("E0308"))]);
}

#[test]
fn an_opaque_future_keeps_its_nested_await_operand() {
    check_future_imports(NESTED_FUTURE, &[("send", None), ("wrong", Some("E0277"))]);
}

#[test]
fn a_hidden_type_producer_reads_the_module_declared_by_its_macro() {
    const SOURCE: &str = r#"
mod local { pub type Mask = bool; }
macro_rules! mask {
    ($value:expr) => {{
        mod local { pub type Mask = u8; }
        let _: local::Mask = $value;
    }};
}
fn producer() -> impl Sized { mask!(1u8); 1u8 }
fn control() -> impl Sized { mask!(false); 1u8 }
fn require_send<T: Send>(_: T) {}
fn caller() { require_send(producer()); }
fn main() {}
"#;
    check_future_imports(SOURCE, &[("caller", None), ("control", Some("E0308"))]);
}

#[test]
fn boxed_return_values_keep_the_future_output_and_stream_item() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let toolchain = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("open the installed toolchain");
    let files = [("/boxed/src/main.rs".to_owned(), PROGRAM.to_owned())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
    let user = UserCrate { name: "boxed", dir: "/boxed/", root: "/boxed/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &toolchain).expect("toolchain and user crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve the boxed-return crate");
    let sources = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front end");
    let bodies = world.bodies(alloc);
    let body = |name: &str| {
        bodies.iter().find_map(|&(b, _)| {
            let written = match b {
                BodyRef::Free(k) => names.item_text(k)?.name?,
                BodyRef::Member { name, .. } => name,
            };
            (written == name).then_some(b)
        }).unwrap_or_else(|| panic!("missing body for `{name}`"))
    };
    for name in ["immediate", "suspending", "boxed", "boxed_stream", "listed", "inherited"] {
        let (types, borrows) = world.check_borrows(body(name), alloc);
        assert!(types.is_clean() && types.findings.is_empty() && types.undecided.is_empty(), "{name}: {types:?}");
        assert!(types.reads.is_some(), "{name}: {types:?}");
        assert!(borrows.is_clean(), "{name}: {borrows:?}");
    }
    // Two instances of either alias with different associated types cannot be interchanged.
    for name in ["different_output", "different_item"] {
        let b = body(name);
        let BodyRef::Free(k) = b else { panic!("expected a free fn for {name}") };
        let item = names.item_text(k).expect("the fn's text").text;
        let types = world.check(b, alloc);
        assert!(types.gaps.is_empty() && types.refused.is_none() && types.undecided.is_empty(), "{name}: {types:?}");
        let [diag] = types.diagnostics.as_slice() else { panic!("{name}: expected one type mismatch, got {types:?}") };
        assert_eq!(diag.code, "E0308", "{name}: {types:?}");
        assert_eq!(item.get(diag.start as usize..diag.end as usize), Some("value"), "{name}: {types:?}");
    }
    let types = world.check(body("absent"), alloc);
    assert!(types.gaps.is_empty() && types.refused.is_none() && types.undecided.is_empty(), "{types:?}");
    let [diag] = types.diagnostics.as_slice() else { panic!("expected an unmet Send obligation: {types:?}") };
    assert_eq!(diag.code, "E0277", "{types:?}");
    assert!(diag.text.contains("Send"), "{types:?}");
}

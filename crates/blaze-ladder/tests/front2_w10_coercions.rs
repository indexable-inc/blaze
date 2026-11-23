//! Two false errors of tempo's, as small programs rustc accepts, each with a control that must
//! still be an error.
//!
//! 1. A closure expected as a trait object takes the object's output as its return type, so
//!    its body is a coercion site to it (rustc `deduce_closure_signature` on `ty::Dynamic`, then
//!    `sig_of_closure_with_expectation`; the Reference, "Type coercions", "Coercion sites").
//!    tempo's crates/eyre lib.rs:42, `eyre::set_hook(Box::new(|_| Box::new(ErrorHandler)))`,
//!    was "found `Box<ErrorHandler>`, expected `Box<dyn ..>`" and a failed `FnOnce::Output`
//!    projection. Control: a handler type that does not implement the trait (E0277).
//! 2. Inside `impl Tr for T`, the trait `Tr` is in scope for method calls without a `use`
//!    (rustc_resolve's `traits_in_scope` adds `current_trait_ref`). tempo's zone_sequencer
//!    settlement.rs:96 and :101, `error.fmt(formatter)` on a `&eyre::Report` inside `impl
//!    fmt::Display for BatchSubmitError` with only `std::fmt` imported, were "found `&Report`,
//!    about `fmt`". Every `fmt` here is clean; the control is the same call in a free fn, where
//!    neither `Display` nor `Debug` is in scope (E0599).
//!
//! Prints `<name>: clean` or `<name>: <code>` per body.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::fmt;
trait Source {}
trait Handler {}
struct Good;
impl Handler for Good {}
struct NotHandler;
type Hook = Box<dyn Fn(&dyn Source) -> Box<dyn Handler> + Send + Sync + 'static>;
fn set_hook(_: Hook) {}
fn install() {
    set_hook(Box::new(|_| Box::new(Good)));
}
fn install_not_handler() {
    set_hook(Box::new(|_| Box::new(NotHandler)));
}
mod inner {
    pub struct Report;
    impl core::fmt::Display for Report {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str("report") }
    }
    impl core::fmt::Debug for Report {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str("Report") }
    }
}
enum Wrapper { Other(inner::Report) }
impl fmt::Display for Wrapper {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(error) => error.fmt(formatter),
        }
    }
}
fn free_fmt(error: &inner::Report, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    error.fmt(formatter)
}
fn main() {}
"#;

#[test]
fn closure_output_from_object_and_impl_trait_in_scope() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/w10/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "w10", dir: "/w10/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture and standard library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let bodies = world.bodies(alloc);
    let named = |body: BodyRef<'_>| -> Option<String> {
        match body {
            BodyRef::Free(key) => names.item_text(key)?.name.map(str::to_owned),
            BodyRef::Member { name, .. } => Some(name.to_owned()),
        }
    };
    // (body name, how many bodies have it, the error codes one of which it must have, or empty
    // for clean). The unsizing control is E0277 in rustc ("the trait bound `NotHandler:
    // Handler` is not satisfied"); a failed unsizing coercion here may be reported as the
    // coercion's E0308 instead: either is the error, a clean verdict is the bug.
    let none: &[&str] = &[];
    for (name, count, error) in [
        ("install", 1, none),
        ("install_not_handler", 1, &["E0277", "E0308"][..]),
        // Report's `Display::fmt` and `Debug::fmt`, and Wrapper's, which calls `Display::fmt`
        // with no `use std::fmt::Display`.
        ("fmt", 3, none),
        ("free_fmt", 1, &["E0599"][..]),
    ] {
        let matching: Vec<BodyRef<'_>> = bodies.iter().map(|&(b, _)| b).filter(|&b| named(b).as_deref() == Some(name)).collect();
        assert_eq!(matching.len(), count, "{name}: bodies of that name");
        for body in matching {
            let v = world.check(body, alloc);
            assert!(v.reads.is_some() && v.gaps.is_empty() && v.refused.is_none() && v.undecided.is_empty(), "{name}: {v:?}");
            if error.is_empty() {
                assert!(v.diagnostics.is_empty(), "{name}: {v:?}");
                println!("{name}: clean");
            } else {
                let found = v.diagnostics.iter().find(|d| error.iter().any(|&code| d.code == code));
                let Some(d) = found else { panic!("{name}: expected one of {error:?}: {v:?}") };
                println!("{name}: {}", d.code);
            }
        }
    }
}

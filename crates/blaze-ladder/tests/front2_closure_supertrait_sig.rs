//! End-to-end tests for closure signature deduction through supertraits of pending trait goals
//! and `dyn SubTrait` trait-object principals (`blaze-typeck/src/rules/closure.rs`).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`. No `rustc` invocations.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const PROGRAM: &str = r#"
use std::future::Future;
use std::pin::Pin;

pub trait ReadinessCheck:
    Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync + 'static
{
}

impl<F> ReadinessCheck for F where
    F: Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync + 'static
{
}

pub fn spawn(_readiness_check: impl ReadinessCheck) {}

fn readiness_ok() {
    spawn(|| Box::pin(async { true }));
}

fn readiness_wrong_return() {
    spawn(|| Box::pin(async { 42u32 }));
}

trait MyTrait {
    fn val(&self) -> i32;
}

struct Concrete(i32);

impl MyTrait for Concrete {
    fn val(&self) -> i32 {
        self.0
    }
}

trait BoxCheck: Fn() -> Box<dyn MyTrait> {}
impl<F: Fn() -> Box<dyn MyTrait>> BoxCheck for F {}

fn take_box_check(_f: impl BoxCheck) {}

fn box_check_ok() {
    take_box_check(|| Box::new(Concrete(7)));
}

fn box_check_wrong_return() {
    take_box_check(|| 42u32);
}

trait Inner<T>: Fn(T) -> usize {}
impl<T, F: Fn(T) -> usize> Inner<T> for F {}

trait Outer<T>: Inner<T> {}
impl<T, F: Inner<T>> Outer<T> for F {}

fn take_outer_string(_f: impl Outer<String>) {}

fn multihop_generic_ok() {
    take_outer_string(|s| s.len());
}

trait SubTrait: Fn(i32) -> Box<dyn MyTrait> {}
impl<F: Fn(i32) -> Box<dyn MyTrait>> SubTrait for F {}

fn take_dyn_sub(_f: Box<dyn SubTrait>) {}
fn take_impl_sub(_f: impl SubTrait) {}

fn dyn_subtrait_ok() {
    take_dyn_sub(Box::new(|x| Box::new(Concrete(x))));
}

fn wrong_arity_err() {
    take_impl_sub(|_, _| Box::new(Concrete(0)));
}

fn main() {}
"#;

#[test]
fn closure_signatures_elaborate_supertraits_end_to_end() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let toolchain = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain")))
        .expect("installed toolchain");
    let files = [("/closure_super/src/main.rs".to_owned(), PROGRAM.to_owned())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
    let user = UserCrate {
        name: "closure_super",
        dir: "/closure_super/",
        root: "/closure_super/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &toolchain).expect("fixture and toolchain");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve");
    let sources = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front end");
    let bodies = world.bodies(alloc);

    let find = |name: &str| {
        bodies
            .iter()
            .find_map(|&(b, _)| match b {
                BodyRef::Free(k) if names.item_text(k)?.name == Some(name) => Some(b),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing body `{name}`"))
    };

    // Positives: both typeck and borrowck must be clean (no gaps, no refusals, no diagnostics).
    for name in ["readiness_ok", "box_check_ok", "multihop_generic_ok", "dyn_subtrait_ok"] {
        let b = find(name);
        let (types, borrows) = world.check_borrows(b, alloc);
        assert!(
            types.is_clean() && types.gaps.is_empty() && types.refused.is_none(),
            "POSITIVE `{name}` typeck failed: {types:?}"
        );
        assert!(
            borrows.refused.is_none() && borrows.findings.is_empty(),
            "POSITIVE `{name}` borrowck failed: {borrows:?}"
        );
    }

    // Negative 1a: wrong future output inside async block fails with E0271 (`Future::Output == bool`).
    let bad_ret = world.check(find("readiness_wrong_return"), alloc);
    let bad_ret_codes: Vec<&str> = bad_ret.diagnostics.iter().map(|d| d.code).collect();
    assert!(
        bad_ret_codes.contains(&"E0271"),
        "NEGATIVE `readiness_wrong_return`: expected E0271, got {bad_ret_codes:?} ({bad_ret:?})"
    );

    // Negative 1b: wrong direct return type fails with E0308.
    let bad_box_ret = world.check(find("box_check_wrong_return"), alloc);
    let bad_box_ret_codes: Vec<&str> = bad_box_ret.diagnostics.iter().map(|d| d.code).collect();
    assert!(
        bad_box_ret_codes.contains(&"E0308"),
        "NEGATIVE `box_check_wrong_return`: expected E0308, got {bad_box_ret_codes:?} ({bad_box_ret:?})"
    );

    // Negative 2: wrong closure arity must fail with an error.
    let bad_arity = world.check(find("wrong_arity_err"), alloc);
    let bad_arity_codes: Vec<&str> = bad_arity.diagnostics.iter().map(|d| d.code).collect();
    assert!(
        !bad_arity_codes.is_empty(),
        "NEGATIVE `wrong_arity_err`: expected error for wrong closure arity, got clean ({bad_arity:?})"
    );
}

//! An auto trait of a REQUIRED trait fn's `impl Trait` (an `async fn` in a trait, or `fn f(&self)
//! -> impl Future`), by Rust's model (front2 rpitit.rs): the trait fn's opaque reveals the
//! synthetic projection `<Self as Tr>::{f}`, whose value in each impl is that impl's fn's return
//! type. The Reference, "Impl trait", "`impl Trait` in traits": each impl's fn gives its own
//! type; through a generic `T: Tr` only the trait's bounds are known.
//!
//! - `declared` (generic receiver, `+ Send` written on the trait's fn): clean by the opaque's
//!   own bound (blaze-solve structural.rs `assemble_auto`), with no reveal.
//! - TWIN `undeclared` (generic receiver, no `+ Send`): E0277 (rustc: "future cannot be sent
//!   between threads safely"), never the old `RequiredTraitFn` gap (`TraitFnInBounds` now).
//! - `concrete_good` (a concrete receiver whose impl's `async fn` holds nothing): clean, the
//!   projection normalized to the impl's future and that future's body revealed.
//! - TWIN `concrete_bad` (the impl's `async fn` holds an `Rc` across an await): E0277 at the
//!   spawn.
//! - `tempo_shape` (tempo's `advance_one`: a generic `D: Driver` awaited in
//!   `drain_all_available`, the spawned block instantiating it with a concrete driver): clean.
//! - TWIN `tempo_shape_bad` (the same with a driver holding an `Rc` across an await): E0277.
//!
//! Before (tempo c0255wave87 and c0340wave89: 13 bodies, `get_transport`, `advance_one`, ..):
//! the check loop tried to file the required fn's body and refused as `RequiredTraitFn`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const PROGRAM: &str = r#"
use std::future::Future;
use std::rc::Rc;

fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }
async fn tick() {}

trait Declared { fn f(&self) -> impl Future<Output = ()> + Send; }
trait Undeclared { fn f(&self) -> impl Future<Output = ()>; }

async fn holds_declared<T: Declared>(t: T) { t.f().await; drop(t); }
async fn holds_undeclared<T: Undeclared>(t: T) { t.f().await; drop(t); }

fn declared<T: Declared + Send + 'static>(t: T) { spawn(holds_declared(t)); }
fn undeclared<T: Undeclared + Send + 'static>(t: T) { spawn(holds_undeclared(t)); }

struct Good;
impl Undeclared for Good { async fn f(&self) { tick().await; } }
struct Bad;
impl Undeclared for Bad { async fn f(&self) { let r = Rc::new(0u8); tick().await; drop(r); } }

fn concrete_good() { spawn(holds_undeclared(Good)); }
fn concrete_bad() { spawn(holds_undeclared(Bad)); }

trait Driver { async fn advance_one(&mut self) -> bool; }
async fn drain_all_available<D: Driver>(d: &mut D) { while d.advance_one().await {} }

struct Real;
impl Driver for Real { async fn advance_one(&mut self) -> bool { tick().await; false } }
struct RcDriver;
impl Driver for RcDriver { async fn advance_one(&mut self) -> bool { let r = Rc::new(0u8); tick().await; drop(r); false } }

fn tempo_shape() { spawn(async { let mut d = Real; drain_all_available(&mut d).await; }); }
fn tempo_shape_bad() { spawn(async { let mut d = RcDriver; drain_all_available(&mut d).await; }); }

fn main() {}
"#;

/// The two seams the batch left UNSURE, each with its twin.
///
/// - `const_own` (an impl fn with its own const parameter, `fn k<const N: usize>(&self) ->
///   [u8; N]`, for a trait fn `-> impl Sized`): the impl's synthetic row is `[u8; ConstParam(0)]`
///   over the impl's generics (none) then the fn's own, so typeck impl_kinds.rs
///   `impl_var_kinds` meets a `ConstParam` past the impl's generics in an associated type's
///   value: that is the value's own parameter, never `Malformed`. A concrete receiver sees the
///   impl's type (`[u8; 3]`): rustc's lint `refining_impl_trait_reachable`, "Callers of methods
///   for types where the implementation is known are able to observe the types written in the
///   impl signature" (typeck rules/rpitit_use.rs; before, the opaque stayed rigid: "E0308 found
///   `impl Trait (..)`, expected `[u8; 3]`", run 2026-11-21 05:56). TWIN `const_own_bad` asks
///   `[u8; 4]` of it: E0308.
/// - `bounded_output` (`fn f(&self) -> impl Future<Output = u32> + Send` in a trait, called on
///   a generic `T`, awaited and spawned): the opaque's own bounds answer both `.await`'s
///   `Output = u32` and `Send` (the synthetic associated type's item bounds are the opaque's
///   predicates, whose `Self` is the opaque, not the projection: blaze-solve alias.rs
///   `push_alias_bounds` reads them as non-self bounds of the projection, so they must not be
///   what answers). TWIN `bounded_output_bad` asks `Output = u64` of it: E0308 or E0271.
const BOUNDS_PROGRAM: &str = r#"
use std::future::Future;

fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }

trait K { fn k<const N: usize>(&self) -> impl Sized; }
struct S;
impl K for S { fn k<const N: usize>(&self) -> [u8; N] { [0; N] } }

fn const_own() { let a: [u8; 3] = S.k::<3>(); drop(a); }
fn const_own_bad() { let a: [u8; 4] = S.k::<3>(); drop(a); }

trait Out { fn f(&self) -> impl Future<Output = u32> + Send; }
async fn read_u32<T: Out>(t: T) -> u32 { let v: u32 = t.f().await; drop(t); v }
async fn read_u64<T: Out>(t: T) -> u64 { let v: u64 = t.f().await; drop(t); v }

fn bounded_output<T: Out + Send + 'static>(t: T) { spawn(read_u32(t)); }
fn bounded_output_bad<T: Out + Send + 'static>(t: T) { spawn(read_u64(t)); }

fn main() {}
"#;

/// A PROVIDED trait fn's `impl Trait`, and one NESTED in a required fn's return type, by the
/// same model (front2 rpitit.rs): the synthetic associated type of a provided fn has a default,
/// the opaque its body defines (rpitit.rs `Twin`), taken by an impl that does not write the fn;
/// an impl that writes it gives its own; a nested opaque's value is the type at its place of the
/// impl fn's return type.
///
/// - `provided_default` (an impl that keeps the default `async fn` body, which holds nothing):
///   clean, by the default body's future.
/// - TWIN `provided_override` (an impl that writes the fn, holding an `Rc` across an await):
///   E0277. Before, the opaque revealed the default body's future at every self type, so this
///   was clean: the cause (the override's own type) is what the twin removes.
/// - TWIN `provided_generic` (a generic receiver): E0277, the projection rigid (rustc: "future
///   cannot be sent between threads safely"). Before, the default body's future leaked: clean.
/// - `nested_good` (`-> Option<impl Future>`, the impl's future holds nothing): clean.
/// - TWIN `nested_bad` (the impl's async block holds an `Rc` across an await): E0277. Before,
///   both stopped on the gap `RequiredTraitFn` (a nested opaque had no synthetic row).
const PROVIDED_PROGRAM: &str = r#"
use std::future::Future;
use std::rc::Rc;

fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }
async fn tick() {}

trait Prov { async fn f(&self) { tick().await; } }
async fn holds_prov<T: Prov>(t: T) { t.f().await; drop(t); }

struct Keeps;
impl Prov for Keeps {}
struct Overrides;
impl Prov for Overrides { async fn f(&self) { let r = Rc::new(0u8); tick().await; drop(r); } }

fn provided_default() { spawn(holds_prov(Keeps)); }
fn provided_override() { spawn(holds_prov(Overrides)); }
fn provided_generic<T: Prov + Send + Sync + 'static>(t: T) { spawn(holds_prov(t)); }

trait Nest { fn f(&self) -> Option<impl Future<Output = ()>>; }
async fn holds_nest<T: Nest>(t: T) { if let Some(fut) = t.f() { fut.await; } drop(t); }

struct NGood;
impl Nest for NGood { fn f(&self) -> Option<impl Future<Output = ()>> { Some(tick()) } }
struct NBad;
impl Nest for NBad { fn f(&self) -> Option<impl Future<Output = ()>> { Some(async { let r = Rc::new(0u8); tick().await; drop(r); }) } }

fn nested_good() { spawn(holds_nest(NGood)); }
fn nested_bad() { spawn(holds_nest(NBad)); }

fn main() {}
"#;

/// On failure panics naming the case and the verdict.
#[test]
fn a_provided_fns_default_and_a_nested_impl_trait_are_revealed_per_impl() {
    let names = ["provided_default", "provided_override", "provided_generic", "nested_good", "nested_bad"];
    let got = check_program(PROVIDED_PROGRAM, &names);
    for (name, g) in names.iter().zip(got.iter()) {
        println!("{name}: clean {}: {}", g.0, g.1);
    }
    pair(&got, 0, "an impl that keeps a provided `async fn`'s default body");
    let generic = &got[2];
    assert!(!generic.0 && generic.1.contains("E0277"), "TWIN: through a generic the provided fn's future is a rigid projection: E0277: {}", generic.1);
    assert!(!generic.1.contains("TraitFnInBounds"), "never the gap: {}", generic.1);
    pair(&got, 3, "`-> Option<impl Future>` of a required fn, an impl whose future holds nothing");
}

/// On failure panics naming the case and the verdict.
#[test]
fn an_impl_fns_own_const_and_a_bounded_output_are_read_by_rust_rules() {
    let got = check_program(BOUNDS_PROGRAM, &["const_own", "const_own_bad", "bounded_output", "bounded_output_bad"]);
    for (name, g) in ["const_own", "const_own_bad", "bounded_output", "bounded_output_bad"].iter().zip(got.iter()) {
        println!("{name}: clean {}: {}", g.0, g.1);
        assert!(!g.1.contains("Malformed"), "{name}: never malformed: {}", g.1);
    }
    assert!(got[0].0, "a concrete receiver sees its impl fn's `[u8; 3]`: {}", got[0].1);
    assert!(!got[1].0 && got[1].1.contains("E0308"), "TWIN: `[u8; 3]` is no `[u8; 4]`: E0308: {}", got[1].1);
    assert!(got[2].0, "`Output = u32` and `Send` by the trait fn's bounds: {}", got[2].1);
    assert!(
        !got[3].0 && (got[3].1.contains("E0308") || got[3].1.contains("E0271")),
        "TWIN: the future's output is `u32`, no `u64`: {}",
        got[3].1
    );
}

/// Each body's (clean, verdict as printed), in [`PROGRAM`].
fn check_bodies(bodies: &[&str]) -> Vec<(bool, String)> {
    check_program(PROGRAM, bodies)
}

/// Each named body of `text` checked: (clean, verdict as printed).
fn check_program(text: &str, bodies: &[&str]) -> Vec<(bool, String)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/rtf/src/main.rs";
    let files = [(root.to_owned(), text.to_owned())];
    let program = UserCrate {
        name: "rtf", dir: "/rtf/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&program, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    bodies
        .iter()
        .map(|&name| {
            let v = world.check(BodyRef::Free(key_of(name)), alloc);
            (v.is_clean(), format!("{v:?}"))
        })
        .collect()
}

/// A clean body and its twin refused with E0277 and never the old named gap.
fn pair(got: &[(bool, String)], at: usize, what: &str) {
    assert!(got[at].0, "{what}: clean: {}", got[at].1);
    let twin = &got[at + 1];
    assert!(!twin.0, "TWIN of {what}: not clean: {}", twin.1);
    assert!(twin.1.contains("E0277"), "TWIN of {what}: E0277, the future is not `Send`: {}", twin.1);
    // The gap was `RequiredTraitFn`; it is `TraitFnInBounds` now, left only for an `impl Trait`
    // in another's bounds.
    assert!(!twin.1.contains("TraitFnInBounds"), "TWIN of {what}: never the gap: {}", twin.1);
}

/// On failure panics naming the pair and the verdict.
#[test]
fn a_required_trait_fns_future_is_send_by_its_bounds_or_by_its_impl() {
    let got = check_bodies(&["declared", "undeclared", "concrete_good", "concrete_bad", "tempo_shape", "tempo_shape_bad"]);
    for (name, g) in ["declared", "undeclared", "concrete_good", "concrete_bad", "tempo_shape", "tempo_shape_bad"].iter().zip(got.iter()) {
        println!("{name}: clean {}", g.0);
    }
    pair(&got, 0, "`+ Send` on the trait's fn, a generic receiver");
    pair(&got, 2, "a concrete receiver whose impl's future holds nothing");
    pair(&got, 4, "tempo's `advance_one` shape with a `Send` driver");
}

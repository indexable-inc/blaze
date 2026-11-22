//! `Clone::clone` at a `Self` whose `Clone` the LANGUAGE gives, no impl's text holding it: a
//! tuple, a closure, a fn item, a fn pointer. Built as a function of its own (blaze-build
//! clone_fn.rs, blaze-ladder clone_shim.rs) and called, BUILT AND RUN.
//!
//! The census of 2026-11-17 04:27 (port27/tempo-run/build-census-g042737.txt) refused 887
//! instances with "callee: resolve NoImpl"; its example is `<impl Clone for SystemRandom>::clone`,
//! a derive on a struct wrapping `()`, whose expansion calls `Clone::clone(&self.0)` at `()`.
//! Instance resolution finds written impls only, and none is written for a tuple.
//!
//! The rules (the Rust Reference, "Special types and traits"): `Clone` is implemented for
//! "function item types, function pointer types, closures that capture no values or only values
//! that implement `Clone`, tuples of types that implement `Clone`"; `Copy` likewise for those of
//! `Copy` parts. core's clone.rs: a `Copy` type's `clone` "is equivalent to `*self`", so a field's
//! own `clone` is not run for a `Copy` tuple (a program sees it); otherwise each field is cloned
//! by its own `clone`, in order.
//!
//! The programs are `#![no_core]` (no toolchain), built and run in this process; the statuses
//! are the arithmetic the comments give.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

/// Overflow wraps: a `no_core` program declares no `panic_const_add_overflow`, and no sum here
/// overflows.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items (core's marker.rs and clone.rs, ops/function.rs), `Clone` and `Copy` of the
/// primitives used, and `LOG`: each written `clone` below appends a digit to it.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}

#[lang = \"clone\"]
trait Clone: Sized {
    fn clone(&self) -> Self;
}

#[lang = \"copy\"]
trait Copy: Clone {}

impl Clone for u32 {
    fn clone(&self) -> u32 {
        *self
    }
}
impl Copy for u32 {}

impl Clone for u8 {
    fn clone(&self) -> u8 {
        *self
    }
}
impl Copy for u8 {}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

static mut LOG: u32 = 0;

/// Not `Copy`: its `clone` appends its digit to `LOG` and adds 10.
struct A(u32);

impl Clone for A {
    fn clone(&self) -> A {
        unsafe {
            LOG = LOG * 10 + self.0;
        }
        A(self.0 + 10)
    }
}

fn log() -> u32 {
    unsafe { LOG }
}
";

/// Builds and runs `HEAD` + `main`: the status, or the refusal on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        // One short line: a refusal's whole text can run past what a log line shows.
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(400).map_or(said.clone(), |(i, _)| said[..i].to_string())
    })
}

/// The census's shape: `#[derive(Clone)]` on a struct wrapping `()`, expanded by hand (the
/// derive writes `::core::clone::Clone::clone(&self.0)`), so `clone` is called at `()`.
/// `S((), 9).clone().1` = 9.
/// Prints `UNIT THROUGH A DERIVE: Ok(9)`.
#[test]
fn unit_cloned_through_a_derive_builds_and_runs() {
    let got = run("
struct S((), u32);

impl Clone for S {
    #[inline]
    fn clone(&self) -> S {
        S(Clone::clone(&self.0), Clone::clone(&self.1))
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let s = S((), 9);
    s.clone().1 as i32
}
");
    println!("UNIT THROUGH A DERIVE: {got:?}");
    assert_eq!(got, Ok(9));
}

/// A tuple of two non-`Copy` values: each field cloned by its own `clone`, in order, once:
/// `LOG` = 12 (1 then 2; twice would be 1212), the clone `(A(11), A(12))`: 12 + 11 + 12 = 35.
/// Prints `NON-COPY TUPLE: Ok(35)`.
#[test]
fn a_non_copy_tuple_clones_each_field_in_order_once() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let t = (A(1), A(2));
    let u = t.clone();
    (log() + u.0.0 + u.1.0) as i32
}
");
    println!("NON-COPY TUPLE: {got:?}");
    assert_eq!(got, Ok(35));
}

/// A `Copy` tuple whose field's written `clone` has a side effect: the tuple's clone is its
/// bytes, and that `clone` is NOT run: `LOG` stays 0, the clone `(C(3), 4)`: 0 + 3 + 4 = 7.
/// Prints `COPY TUPLE: Ok(7)`.
#[test]
fn a_copy_tuple_copies_and_runs_no_field_clone() {
    let got = run("
struct C(u32);

impl Clone for C {
    fn clone(&self) -> C {
        unsafe {
            LOG = LOG + 100;
        }
        C(self.0)
    }
}

impl Copy for C {}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let t = (C(3), 4u8);
    let u = t.clone();
    (log() + u.0.0 + u.1 as u32) as i32
}
");
    println!("COPY TUPLE: {got:?}");
    assert_eq!(got, Ok(7));
}

/// A closure that captures a non-`Copy` value by value: its clone clones the capture by the
/// capture's `clone` (`LOG` = 5, the copy's capture `A(15)`): 5 + 15 + 5 = 25.
/// Prints `CLOSURE: Ok(25)`.
#[test]
fn a_closure_clones_its_capture() {
    let got = run("
fn get(a: &A) -> u32 {
    a.0
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = A(5);
    let f = move || get(&a);
    let g = f.clone();
    (log() + g() + f()) as i32
}
");
    println!("CLOSURE: {got:?}");
    assert_eq!(got, Ok(25));
}

/// A fn item and a fn pointer cloned, then called: 7 + 7 = 14.
/// Prints `FN ITEM AND POINTER: Ok(14)`.
#[test]
fn a_fn_item_and_a_fn_pointer_clone() {
    let got = run("
fn seven() -> u32 {
    7
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let f = seven;
    let g = f.clone();
    let p: fn() -> u32 = seven;
    let q = p.clone();
    (g() + q()) as i32
}
");
    println!("FN ITEM AND POINTER: {got:?}");
    assert_eq!(got, Ok(14));
}

/// A tuple with a field that is not `Clone` is not `Clone`: E0277 at the check, never a build.
/// (`Clone::clone(&t)` by path: `t.clone()` would autoref to `&(N, u8)`'s `clone`.)
/// Prints `NOT CLONE: Err(..E0277..)`.
#[test]
fn a_tuple_with_a_non_clone_field_stays_e0277() {
    let got = run("
struct N;

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let t = (N, 1u8);
    let u: (N, u8) = Clone::clone(&t);
    u.1 as i32
}
");
    println!("NOT CLONE: {got:?}");
    let said = got.expect_err("a tuple with a non-`Clone` field is not `Clone`");
    assert!(said.contains("E0277") || said.contains("Unsatisfied"), "{said}");
}

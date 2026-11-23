//! PROBE (census 14, Brief A): auto-trait goals that go through core's `Atomic<T>`, about 360
//! tempo bodies ("E0283 cannot decide `DefaultCallsite: Sync`", E0277 `Send`, `Unpin` of
//! `TempoTxEnvelope`, `TcpStream`, `EthStorage`, ..). Every failing leaf holds core's
//! `Atomic<T>` (through `AtomicU8`, `AtomicUsize`, `AtomicPtr`, `Once`, `OnceLock`). The
//! mechanism is NOT named yet: this file's pass/fail pattern names it, and the fix is written
//! only after its lines come back.
//!
//! The rule (the Reference, "Special types and traits", "Auto traits"): "If no explicit
//! implementation or negative implementation is written out for an auto trait for a given type,
//! then the compiler implements it automatically according to the following rules: `&T`, `&mut
//! T`, `*const T`, `*mut T`, `[T; n]`, and `[T]` implement the trait if `T` does. [..] Structs,
//! enums, unions, and tuples implement the trait if all of their fields do." And: "For generic
//! types (counting the built-in types above as generic over `T`), if a generic implementation is
//! available, then the compiler does not automatically implement it for types that could use the
//! implementation except that they do not meet the requisite trait bounds."
//!
//! The library (nightly-2026-10-27, library/core/src/sync/atomic.rs):
//! "pub impl(self) unsafe trait AtomicPrimitive: Sized + Copy { type Storage: Sized; }";
//! "macro impl_atomic_primitive { (@impl [$($T:ident)?] $Primitive:ty as $Storage:ident<$Operand:ty>,
//! $cfg:meta) => { #[cfg($cfg)] unsafe impl $(<$T>)? AtomicPrimitive for $Primitive { type
//! Storage = private::$Storage<$Operand>; } }, ([..] size($size:literal)) => {
//! impl_atomic_primitive!(@impl [..], target_has_atomic_load_store = $size); }, .. }";
//! "impl_atomic_primitive!([] u8 as Align1<u8>, size(\"8\"));";
//! "#[cfg(target_pointer_width = \"64\")] impl_atomic_primitive!([] usize as Align8<usize>, size(\"ptr\"));"
//! (and the same for "16" and "32", and for "[T] *mut T as Align8<*mut T>");
//! "pub struct Atomic<T: AtomicPrimitive> { v: UnsafeCell<T::Storage> }";
//! "unsafe impl<T: AtomicPrimitive> Send for Atomic<T> {}"; "unsafe impl<T: AtomicPrimitive> Sync
//! for Atomic<T> {}". library/core/src/cell.rs: "impl<T: ?Sized> !Sync for UnsafeCell<T> {}" (no
//! impl of `Unpin` or `Send`: those are structural). library/std/src/sys/sync/once/queue.rs:67
//! "state_and_queue: Atomic<*mut ()>".
//!
//! So EVERY fn below must check clean (each goal holds by the rules quoted). What each
//! hypothesis predicts (the integrator sends back every printed line; the pattern decides):
//!
//! - H1, core's `AtomicPrimitive` impls are not lowered or not selected (the cfg'd macros-2.0
//!   expansion): `lib: prim_u8` is NOT clean (E0277 when the impl is missing; E0283 when the
//!   `#[cfg(..)]`'d invocations are all kept, so `usize` and `*mut T` have three impls each),
//!   while every `own:` line is clean; the `macro:` lines name the form: `q1` (macros 2.0, no
//!   cfg), `q2` (`#[cfg($cfg)]` from a simple meta), `q3` (core's two-level form building
//!   `target_has_atomic_load_store = $size`), `q4` (outer `#[cfg]` on invocations, two false),
//!   `q5` (all of core's form: `[T] *mut T` and outer cfg).
//! - H2, a relevant impl whose self type is `Blanket` (an unread header lowered to `Error`, or a
//!   parameter) disqualifies the builtin auto candidate (blaze-solve structural.rs
//!   `assemble_auto`): `own: plain_send`, `plain_sync` or `plain_unpin` (a struct of the crate
//!   with a `u8` and a `&'static str`: no atomic anywhere) is NOT clean, and so are `lib:
//!   unsafecell_u8_unpin` and `lib: maybeuninit_u8_unpin`; `lib: prim_u8` is clean.
//! - H3, a projection self type gets no auto candidate (structural.rs `TyKind::Alias(..) =>
//!   return Ok(())`, the projection not normalized first): `own: plain_*` clean but `own:
//!   alias_field_unpin` (a field `UnsafeCell<<u8 as Pr>::S>`) and `own: alias_unpin` NOT clean;
//!   `lib: storage_u8_unpin` NOT clean while `lib: prim_u8` is clean.
//! - None of the three: every `own:` and `macro:` line clean, the `lib:` atomic lines not: the
//!   cause is core-specific; `restrict:` says whether it is the `impl(self)` trait head.
//!
//! Expected lines when fixed: every line ends in ": clean", for instance
//! "lib: sync_atomic_u8: clean", "own: at_sync: clean", "macro: q5_mut_ptr: clean".
//! Before the fix (the brief's report from the census): at least "lib: sync_atomic_u8: diags
//! [..E0283..`AtomicU8: Sync`..]" or an E0277 line for the `Atomic` goals.
//!
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder
//! test '--test front2_auto_atomic -- --nocapture'`. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_const_open.rs does. NOT RUN by the writer (a
//! write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: the same
/// harness as tests/front2_const_open.rs `check_fns` (copied: tests share no module).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break match check_body(&mut records, &body, &scope) {
                        Ok(done) if done.diags.is_empty() => "clean".to_string(),
                        Ok(done) => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// Prints every outcome as `<what>: <fn>: <outcome>` FIRST (the probe's whole answer is the
/// pattern of lines), then asserts each is `clean`.
fn expect_clean(what: &str, got: &[(String, String)]) {
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{what}: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let bad: Vec<&String> = lines.iter().filter(|l| !l.ends_with(": clean")).collect();
    assert!(bad.is_empty(), "{} of {} not clean (each goal holds by the Reference's auto-trait rule):\n{}", bad.len(), lines.len(), lines.join("\n"));
}

/// The library's own types. Paths are written whole in each body (no `use` at the top), so a
/// path the front end cannot resolve stops only its own fn and prints as its line.
const LIB: &str = "#![feature(atomic_internals)]
fn is_send<T: ?Sized + Send>() {}
fn is_sync<T: ?Sized + Sync>() {}
fn is_unpin<T: ?Sized + Unpin>() {}
fn is_prim<T: core::sync::atomic::AtomicPrimitive>() {}
trait C: Sync {}
struct M { c: &'static dyn C }
struct D { a: core::sync::atomic::AtomicU8, m: &'static M, n: core::sync::atomic::AtomicPtr<D> }
fn prim_u8() { is_prim::<u8>(); }
fn prim_usize() { is_prim::<usize>(); }
fn prim_mut_ptr() { is_prim::<*mut ()>(); }
fn storage_u8_unpin() { is_unpin::<<u8 as core::sync::atomic::AtomicPrimitive>::Storage>(); }
fn storage_usize_unpin() { is_unpin::<<usize as core::sync::atomic::AtomicPrimitive>::Storage>(); }
fn unsafecell_u8_unpin() { is_unpin::<core::cell::UnsafeCell<u8>>(); }
fn unsafecell_u8_send() { is_send::<core::cell::UnsafeCell<u8>>(); }
fn maybeuninit_u8_unpin() { is_unpin::<core::mem::MaybeUninit<u8>>(); }
fn sync_atomic_u8() { is_sync::<core::sync::atomic::AtomicU8>(); }
fn send_atomic_u8() { is_send::<core::sync::atomic::AtomicU8>(); }
fn unpin_atomic_u8() { is_unpin::<core::sync::atomic::AtomicU8>(); }
fn sync_atomic_usize() { is_sync::<core::sync::atomic::AtomicUsize>(); }
fn unpin_atomic_usize() { is_unpin::<core::sync::atomic::AtomicUsize>(); }
fn sync_atomic_ptr() { is_sync::<core::sync::atomic::AtomicPtr<()>>(); }
fn unpin_atomic_ptr() { is_unpin::<core::sync::atomic::AtomicPtr<()>>(); }
fn sync_tracing_shape() { is_sync::<D>(); }
fn main() {}
";

/// The crate's own replicas, no macro and no atomic: the solver's rules alone.
const OWN: &str = "use core::cell::UnsafeCell;
fn is_send<T: ?Sized + Send>() {}
fn is_sync<T: ?Sized + Sync>() {}
fn is_unpin<T: ?Sized + Unpin>() {}
struct Plain { a: u8, b: &'static str }
pub trait Pr { type S; }
impl Pr for u8 { type S = u16; }
struct H<T: Pr> { v: UnsafeCell<T::S> }
pub unsafe trait Prim: Sized + Copy { type Storage: Sized; }
#[repr(C, align(1))]
pub struct B1<T>(T);
#[repr(C, align(8))]
pub struct B8<T>(T);
unsafe impl Prim for u8 { type Storage = B1<u8>; }
unsafe impl<T> Prim for *mut T { type Storage = B8<*mut T>; }
#[repr(C)]
pub struct At<T: Prim> { v: UnsafeCell<T::Storage> }
unsafe impl<T: Prim> Send for At<T> {}
unsafe impl<T: Prim> Sync for At<T> {}
trait C: Sync {}
struct M { c: &'static dyn C }
struct D { a: At<u8>, m: &'static M, n: At<*mut D> }
fn plain_send() { is_send::<Plain>(); }
fn plain_sync() { is_sync::<Plain>(); }
fn plain_unpin() { is_unpin::<Plain>(); }
fn alias_unpin() { is_unpin::<<u8 as Pr>::S>(); }
fn alias_field_unpin() { is_unpin::<H<u8>>(); }
fn alias_field_send() { is_send::<H<u8>>(); }
fn at_sync() { is_sync::<At<u8>>(); }
fn at_send() { is_send::<At<u8>>(); }
fn at_unpin() { is_unpin::<At<u8>>(); }
fn at_ptr_sync() { is_sync::<At<*mut ()>>(); }
fn at_ptr_unpin() { is_unpin::<At<*mut ()>>(); }
fn dyn_sync() { is_sync::<dyn C>(); }
fn tracing_shape_sync() { is_sync::<D>(); }
fn main() {}
";

/// The library's macros-2.0 form, rebuilt one step at a time, each step on its own trait.
const MACRO: &str = "#![feature(decl_macro)]
fn is_unpin<T: ?Sized + Unpin>() {}
fn is_q1<T: Q1>() {}
fn is_q2<T: Q2>() {}
fn is_q3<T: Q3>() {}
fn is_q4<T: Q4>() {}
fn is_q5<T: Q5>() {}
pub trait Q1 { type S; }
pub trait Q2 { type S; }
pub trait Q3 { type S; }
pub trait Q4 { type S; }
pub unsafe trait Q5: Sized + Copy { type S: Sized; }
mod private {
    pub struct B1<T>(T);
    pub struct B2<T>(T);
    pub struct B4<T>(T);
    pub struct B8<T>(T);
}
macro m1($p:ty as $s:ident<$o:ty>) {
    impl Q1 for $p { type S = private::$s<$o>; }
}
m1!(u8 as B1<u8>);
macro m2($p:ty, $cfg:meta) {
    #[cfg($cfg)]
    impl Q2 for $p { type S = u16; }
}
m2!(u8, target_pointer_width = \"64\");
macro m3 {
    (@impl [$($T:ident)?] $P:ty as $S:ident<$O:ty>, $cfg:meta) => {
        #[cfg($cfg)]
        impl $(<$T>)? Q3 for $P { type S = private::$S<$O>; }
    },
    ([$($T:ident)?] $P:ty as $S:ident<$O:ty>, size($size:literal)) => {
        m3!(@impl [$($T)?] $P as $S<$O>, target_has_atomic_load_store = $size);
    },
}
m3!([] u8 as B1<u8>, size(\"8\"));
macro m4($p:ty as $s:ident<$o:ty>) {
    impl Q4 for $p { type S = private::$s<$o>; }
}
#[cfg(target_pointer_width = \"16\")]
m4!(usize as B2<usize>);
#[cfg(target_pointer_width = \"32\")]
m4!(usize as B4<usize>);
#[cfg(target_pointer_width = \"64\")]
m4!(usize as B8<usize>);
macro m5 {
    (@impl [$($T:ident)?] $P:ty as $S:ident<$O:ty>, $cfg:meta) => {
        #[cfg($cfg)]
        unsafe impl $(<$T>)? Q5 for $P { type S = private::$S<$O>; }
    },
    ([$($T:ident)?] $P:ty as $S:ident<$O:ty>, size($size:literal)) => {
        m5!(@impl [$($T)?] $P as $S<$O>, target_has_atomic_load_store = $size);
    },
}
#[cfg(target_pointer_width = \"16\")]
m5!([] usize as B2<usize>, size(\"ptr\"));
#[cfg(target_pointer_width = \"32\")]
m5!([] usize as B4<usize>, size(\"ptr\"));
#[cfg(target_pointer_width = \"64\")]
m5!([] usize as B8<usize>, size(\"ptr\"));
#[cfg(target_pointer_width = \"16\")]
m5!([T] *mut T as B2<*mut T>, size(\"ptr\"));
#[cfg(target_pointer_width = \"32\")]
m5!([T] *mut T as B4<*mut T>, size(\"ptr\"));
#[cfg(target_pointer_width = \"64\")]
m5!([T] *mut T as B8<*mut T>, size(\"ptr\"));
fn q1_u8() { is_q1::<u8>(); }
fn q2_u8() { is_q2::<u8>(); }
fn q3_u8() { is_q3::<u8>(); }
fn q4_usize() { is_q4::<usize>(); }
fn q5_usize() { is_q5::<usize>(); }
fn q5_mut_ptr() { is_q5::<*mut ()>(); }
fn q5_storage_unpin() { is_unpin::<<usize as Q5>::S>(); }
fn main() {}
";

/// The trait head core writes: `pub impl(self) unsafe trait` (RFC 3323's impl restriction,
/// core's `#![feature(impl_restriction)]`; it restricts where impls may be written, nothing
/// about which goals hold).
const RESTRICT: &str = "#![feature(impl_restriction)]
fn is_unpin<T: ?Sized + Unpin>() {}
fn is_r<T: R>() {}
pub impl(self) unsafe trait R: Sized + Copy { type S: Sized; }
unsafe impl R for u8 { type S = u16; }
fn restricted_u8() { is_r::<u8>(); }
fn restricted_storage_unpin() { is_unpin::<<u8 as R>::S>(); }
fn main() {}
";

/// The brief's goals on core's and std's own types. Expected: every line "lib: <fn>: clean".
#[test]
fn lib_atomic_goals() {
    let fns = [
        "sync_atomic_u8",
        "send_atomic_u8",
        "unpin_atomic_u8",
        "sync_atomic_usize",
        "unpin_atomic_usize",
        "sync_atomic_ptr",
        "unpin_atomic_ptr",
        "sync_tracing_shape",
    ];
    // Rust std's `Once` and `OnceLock` are not asked here: this file's crate is a blaze edition,
    // where `std` is bstd and Rust's std is unreachable (blaze-unit-graph implicit.rs: "bstd;
    // Rust's `std` none"); bstd's `Once<T>` takes one parameter and its `sync` has no
    // `OnceLock`. Their goals live in tests/front2_auto_shape.rs `rust_edition_std` (edition 2024).
    expect_clean("lib", &check_fns(LIB, &fns));
}

/// H1 against H3 on core's own trait. Expected: every line "lib: <fn>: clean".
#[test]
fn lib_atomic_primitive() {
    let fns = ["prim_u8", "prim_usize", "prim_mut_ptr", "storage_u8_unpin", "storage_usize_unpin"];
    expect_clean("lib", &check_fns(LIB, &fns));
}

/// H2's control on core's types with no atomic. Expected: every line "lib: <fn>: clean".
#[test]
fn lib_no_atomic() {
    let fns = ["unsafecell_u8_unpin", "unsafecell_u8_send", "maybeuninit_u8_unpin"];
    expect_clean("lib", &check_fns(LIB, &fns));
}

/// H2 and H3 on the crate's own types. Expected: every line "own: <fn>: clean".
#[test]
fn own_replicas() {
    let fns = [
        "plain_send",
        "plain_sync",
        "plain_unpin",
        "alias_unpin",
        "alias_field_unpin",
        "alias_field_send",
        "at_sync",
        "at_send",
        "at_unpin",
        "at_ptr_sync",
        "at_ptr_unpin",
        "dyn_sync",
        "tracing_shape_sync",
    ];
    expect_clean("own", &check_fns(OWN, &fns));
}

/// H1's mechanism, form by form. Expected: every line "macro: <fn>: clean".
#[test]
fn macro_forms() {
    let fns = ["q1_u8", "q2_u8", "q3_u8", "q4_usize", "q5_usize", "q5_mut_ptr", "q5_storage_unpin"];
    expect_clean("macro", &check_fns(MACRO, &fns));
}

/// The `impl(self)` trait head. Expected: every line "restrict: <fn>: clean".
#[test]
fn restricted_trait_head() {
    let fns = ["restricted_u8", "restricted_storage_unpin"];
    expect_clean("restrict", &check_fns(RESTRICT, &fns));
}

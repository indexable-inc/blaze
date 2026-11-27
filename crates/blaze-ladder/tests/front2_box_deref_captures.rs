//! RFC 2229 disjoint field captures through `Box` dereferences (`&(*b).field`, `&mut (*b).field`,
//! `&(*b)`), verified end-to-end across borrow checking (`ProjectWorld::check_borrows` with real
//! `std::boxed::Box`) AND native codegen + execution (`run_with` on the new front end).
//!
//! Covers:
//! - Borrowck positive:
//!   - `tcp-tunnel/src/transport.rs` shape: an `async` block mutably borrows `proxy.sender` while
//!     outer code concurrently borrows `proxy.connection` via two-phase `&mut` method call.
//!   - Closure mutably borrows `p.a` while outer code mutably borrows `p.b` (including two-phase).
//!   - Direct `*b` method call inside a closure / async block (`b.push(1)` with `b: Box<Vec<_>>`).
//!   - Nested `Box<Box<Pair>>` disjoint field borrows.
//!   - Unsized `Box<[i32]>` captured by shared/mut ref inside closure/async block.
//!   - `move` closure and moving a non-`Copy` field out of `Box` still capture whole `Box`.
//! - Borrowck negative controls (rule 23):
//!   - Closure or `async` block mutably borrows `p.a` while outer code ALSO mutably borrows `p.a`
//!     -> E0499.
//!   - `move || p.a.len()` moves whole `p`, so using `p.b` afterwards -> E0382.
//! - Native codegen + execution:
//!   - Disjoint `&(*p).a`, `&mut (*p).a` in closures and `async` blocks while mutating `p.b`.
//!   - Direct `*p` method call (`p.add(n)` / `p.val()`) in closure and `async` block.
//!   - Nested `MyBox<MyBox<Pair>>` disjoint capture.
//!   - Unsized `MyBox<[i32]>` (`CoerceUnsized`) captured by ref/mut ref and indexed/read in closure.
//!   - `move ||` and non-`Copy` field move out of `MyBox` with drop/free accounting.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const BORROW_SOURCE: &str = r#"
pub struct Channel {
    sent: u32,
}
impl Channel {
    fn send(&mut self, v: u32) {
        self.sent += v;
    }
    fn wait_idle(&mut self) -> u32 {
        self.sent
    }
}

pub struct Proxy {
    sender: Channel,
    connection: Channel,
}

pub struct Pair {
    a: Vec<u32>,
    b: Vec<u32>,
}

fn take_vec(_: Vec<u32>) {}

/// Positive 1 (closure): mutable borrow of `(*p).a` in closure + concurrent two-phase mutable
/// borrow of `(*p).b` outside, then uses `p` after.
fn closure_disjoint_box_fields(mut p: Box<Pair>) -> usize {
    let mut c = || p.a.push(1);
    p.b.push(2);
    c();
    p.a.len() + p.b.len()
}

/// Positive 2 (exact `tcp-tunnel/src/transport.rs` async block shape): `async` block holds
/// `&mut (*proxy).sender` while outer code calls `proxy.connection.wait_idle()`.
fn async_disjoint_box_fields(mut proxy: Box<Proxy>) -> u32 {
    let fut = async {
        proxy.sender.send(7);
    };
    let idle = proxy.connection.wait_idle();
    drop(fut);
    proxy.sender.wait_idle() + idle
}

/// Positive 3 (direct `*b` capture): closure calls `b.push(1)` on `b: Box<Vec<u32>>` (capturing
/// `&mut (*b)` directly via `Box` deref).
fn closure_direct_box_deref_method(mut b: Box<Vec<u32>>) -> usize {
    let mut c = || b.push(42);
    c();
    b.len()
}

/// Positive 4 (nested `Box<Box<Pair>>`): disjoint borrows through two `Box` derefs.
fn nested_box_disjoint_fields(mut pp: Box<Box<Pair>>) -> usize {
    let mut c = || pp.a.push(1);
    pp.b.push(2);
    c();
    pp.a.len() + pp.b.len()
}

/// Positive 5 (unsized `Box<[i32]>`): closure borrows `(*s)` directly.
fn unsized_box_slice_capture(mut s: Box<[i32]>) -> i32 {
    let mut c = || {
        s[0] += 1;
    };
    c();
    s[0]
}

/// Positive 6 (`move` closure / non-Copy move out of `Box`): captures whole `Box` cleanly.
fn move_non_copy_out_of_box(p: Box<Pair>) {
    let c = || take_vec(p.a);
    c();
}

/// Negative control 1 (closure): same field `(*p).a` mutably borrowed by both closure and outer
/// two-phase method call while closure is still live -> E0499.
fn closure_same_box_field_conflict(mut p: Box<Pair>) {
    let mut c = || p.a.push(1);
    p.a.push(2);
    c();
}

/// Negative control 2 (`async` block): same field `(*proxy).sender` mutably borrowed by live
/// future and outer two-phase method call -> E0499.
fn async_same_box_field_conflict(mut proxy: Box<Proxy>) {
    let fut = async {
        proxy.sender.send(7);
    };
    let _ = proxy.sender.wait_idle();
    drop(fut);
}

/// Negative control 3 (`move` closure): `move || p.a.len()` moves the whole `Box`, so reading
/// `p.b` afterwards is a use-after-move -> E0382.
fn move_box_then_use_sibling_conflict(p: Box<Pair>) -> usize {
    let c = move || p.a.len();
    let n = c();
    n + p.b.len()
}

fn main() {}
"#;

#[derive(Debug)]
struct Verdict {
    name: &'static str,
    types_clean: bool,
    borrow_codes: Vec<&'static str>,
    stopped: bool,
}

fn check_borrow_cases(wanted: &[&'static str]) -> Vec<Verdict> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/boxcap/src/main.rs";
    let files = [(root.to_owned(), BORROW_SOURCE.to_owned())];
    let user = UserCrate {
        name: "boxcap",
        dir: "/boxcap/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: BORROW_SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut out = Vec::new();
    for &name in wanted {
        let key = names
            .items(0)
            .find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key))
            .expect(name);
        let (v, b) = world.check_borrows(BodyRef::Free(key), alloc);
        let verdict = Verdict {
            name,
            types_clean: v.is_clean() && v.reads.is_some(),
            borrow_codes: b.findings.iter().map(|d| d.code).collect(),
            stopped: !(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none()) || b.refused.is_some(),
        };
        println!(
            "{name}: types_clean={} borrow_codes={:?} stopped={}",
            verdict.types_clean, verdict.borrow_codes, verdict.stopped
        );
        if verdict.stopped {
            println!("  stopped by: gaps={:?} undecided={:?} refused={:?} borrow_refused={:?}", v.gaps, v.undecided, v.refused, b.refused);
        }
        out.push(verdict);
    }
    out
}

#[test]
fn borrowck_disjoint_box_deref_captures_and_negative_controls() {
    let rows = check_borrow_cases(&[
        "closure_disjoint_box_fields",
        "async_disjoint_box_fields",
        "closure_direct_box_deref_method",
        "nested_box_disjoint_fields",
        "unsized_box_slice_capture",
        "move_non_copy_out_of_box",
        "closure_same_box_field_conflict",
        "async_same_box_field_conflict",
        "move_box_then_use_sibling_conflict",
    ]);

    for name in [
        "closure_disjoint_box_fields",
        "async_disjoint_box_fields",
        "closure_direct_box_deref_method",
        "nested_box_disjoint_fields",
        "unsized_box_slice_capture",
        "move_non_copy_out_of_box",
    ] {
        let r = rows.iter().find(|r| r.name == name).expect(name);
        assert!(r.types_clean && !r.stopped && r.borrow_codes.is_empty(), "POSITIVE `{name}` must pass cleanly: {r:?}");
    }

    for name in ["closure_same_box_field_conflict", "async_same_box_field_conflict"] {
        let r = rows.iter().find(|r| r.name == name).expect(name);
        assert!(r.types_clean && !r.stopped, "NEGATIVE `{name}` must typecheck without refusal: {r:?}");
        assert!(r.borrow_codes.contains(&"E0499"), "NEGATIVE `{name}` must report E0499: {r:?}");
    }

    let r = rows.iter().find(|r| r.name == "move_box_then_use_sibling_conflict").expect("move conflict");
    assert!(r.types_clean && !r.stopped, "NEGATIVE move conflict must typecheck without refusal: {r:?}");
    assert!(r.borrow_codes.contains(&"E0382"), "NEGATIVE move conflict must report E0382: {r:?}");
}

const NEW: NewFront<'static> =
    NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const FUTURE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

const RUN_HEAD: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use prelude::*;

#[lang = "deref"]
pub trait Deref {
    #[lang = "deref_target"]
    type Target: PointeeSized;
    fn deref(&self) -> &Self::Target;
}

#[lang = "deref_mut"]
pub trait DerefMut: Deref {
    fn deref_mut(&mut self) -> &mut Self::Target;
}

#[rustc_intrinsic]
unsafe fn size_of_val<T: PointeeSized>(ptr: *const T) -> usize;

#[rustc_intrinsic]
unsafe fn write_via_move<T>(dst: *mut T, value: T);

#[rustc_intrinsic]
const fn forget<T>(_: T);

static mut ARENA: [u64; 256] = [0; 256];
static mut NEXT_SLOT: usize = 0;
static mut FREE_COUNT: i32 = 0;

#[lang = "owned_box"]
struct MyBox<T: PointeeSized>(*mut T);

impl<T> MyBox<T> {
    fn new(v: T) -> MyBox<T> {
        let sz = size_of::<T>();
        if sz == 0 {
            forget(v);
            return MyBox(8usize as *mut T);
        }
        unsafe {
            let words = (sz + 7) >> 3;
            let base = &raw mut ARENA as usize;
            let slot = (base + NEXT_SLOT * 8) as *mut T;
            NEXT_SLOT = NEXT_SLOT + words;
            write_via_move(slot, v);
            MyBox(slot)
        }
    }
}

impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<MyBox<U>> for MyBox<T> {}
impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*mut U> for *mut T {}

impl<T: PointeeSized> Deref for MyBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &**self
    }
}

impl<T: PointeeSized> DerefMut for MyBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut **self
    }
}

impl<T: PointeeSized> Drop for MyBox<T> {
    fn drop(&mut self) {
        let sz = unsafe { size_of_val(self.0 as *const T) };
        if sz != 0 {
            unsafe {
                FREE_COUNT = FREE_COUNT + 1;
            }
        }
    }
}

fn poll_once<F: future::Future>(mut f: F) -> F::Output {
    let mut wakes = 0u32;
    let mut cx = future::Context { wakes: &mut wakes };
    match future::Future::poll(future::Pin { pointer: &mut f }, &mut cx) {
        future::Poll::Ready(v) => v,
        future::Poll::Pending => loop {},
    }
}
"#;

fn run(body: &str) -> Result<u8, String> {
    let text = [RUN_HEAD, body].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
        SourceFile { path: "src/future.rs", text: FUTURE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}").replace('\n', " "))
}

/// Codegen + run: closure and async block capture `&(*p).a` and `&mut (*p).a` through `Box` deref
/// while outer code simultaneously mutates `p.b`.
#[test]
fn run_disjoint_field_captures_through_box_in_closure_and_async() {
    let got = run(r#"
struct Pair {
    a: i32,
    b: i32,
}

impl Pair {
    fn bump_a(&mut self, delta: i32) {
        self.a = self.a + delta;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut p = MyBox::new(Pair { a: 10, b: 20 });
    {
        let mut inc_a = || {
            p.a = p.a + 5;
        };
        p.b = p.b + 7;
        inc_a();
    }
    let read_a = || p.a;
    let x = read_a();
    let y = {
        let fut = async {
            p.a = p.a + 3;
            p.a
        };
        p.b = p.b + 4;
        poll_once(fut)
    };
    // Direct `*p` method capture in closure:
    {
        let mut m = || p.bump_a(2);
        m();
    }
    // x = 15, y = 18, p.a = 20, p.b = 31 -> 15 + 18 + 20 + 31 = 84.
    x + y + p.a + p.b
}
"#);
    assert_eq!(got, Ok(84));
}

/// Codegen + run: nested `MyBox<MyBox<Pair>>` disjoint field capture (`&mut (**pp).a`) while
/// outer code mutates `pp.b`.
#[test]
fn run_nested_box_disjoint_field_capture() {
    let got = run(r#"
struct Pair {
    a: i32,
    b: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut pp = MyBox::new(MyBox::new(Pair { a: 3, b: 9 }));
    {
        let mut c = || {
            pp.a = pp.a * 4;
        };
        pp.b = pp.b + 11;
        c();
    }
    // pp.a = 12, pp.b = 20 -> 32.
    pp.a + pp.b
}
"#);
    assert_eq!(got, Ok(32));
}

/// Codegen + run: unsized `MyBox<[i32]>` captured by shared and mutable reference through `Box`
/// deref (`&(*s)` / `&mut (*s)`).
#[test]
fn run_unsized_box_slice_capture() {
    let got = run(r#"
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut s: MyBox<[i32]> = MyBox::new([4i32, 10i32, 25i32]);
    {
        let mut bump = || {
            s[1] = s[1] + 6;
        };
        bump();
    }
    let sum = || s[0] + s[1] + s[2];
    // 4 + 16 + 25 = 45.
    sum()
}
"#);
    assert_eq!(got, Ok(45));
}

/// Codegen + run: `move` closure and moving a non-`Copy` field out of `MyBox` capture the whole
/// `Box` and run its `Drop` / contents drop exactly once.
#[test]
fn run_move_and_non_copy_field_out_of_box() {
    let got = run(r#"
struct Counted {
    p: *mut i32,
    v: i32,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            *self.p = *self.p + 1;
        }
    }
}

struct Pair {
    a: Counted,
    b: i32,
}

fn take_counted(c: Counted) -> i32 {
    c.v
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut drops = 0i32;
    let dp = &mut drops as *mut i32;
    let v1 = {
        let p = MyBox::new(Pair { a: Counted { p: dp, v: 11 }, b: 5 });
        let c = move || p.a.v + p.b;
        c()
    };
    let v2 = {
        let p = MyBox::new(Pair { a: Counted { p: dp, v: 19 }, b: 7 });
        let c = || take_counted(p.a);
        c()
    };
    let frees = unsafe { FREE_COUNT };
    // v1 = 16, v2 = 19, drops = 2, frees = 2 -> 16 + 19 + 20 + 2 = 57.
    v1 + v2 + drops * 10 + frees
}
"#);
    assert_eq!(got, Ok(57));
}

//! An ASSIGNMENT to a captured binding whose type needs drop, BUILT AND RUN on the new front
//! end. tempo t1530: consensus-config's `read_secret_inner` closure 0 (`SecretBox::init_with_mut(
//! |buf: &mut String| { .. read_result = Err(..); .. })`, `read_result: io::Result<()>` captured
//! by `&mut`) was refused "an assignment to a binding that needs drop and is not owned"
//! (blaze-build walk.rs `Task::Assign`): the closure's body owns no captured binding but a
//! by-value capture of an `FnOnce` body, so a by-reference capture, and a by-value capture of
//! an `FnMut` body, fell to that refusal.
//!
//! The rule (the Reference, "Assignment expressions": "It then has the effect of first dropping
//! the value at the assigned place, unless the place is an uninitialized local variable or an
//! uninitialized field of a local variable", then the value is moved in; "Closure expressions",
//! "Capture modes": a binding assigned in the closure is captured by unique borrow, or by value
//! under `move`): the old value is dropped once at the assignment, wherever its place is, and
//! the new value is dropped by the place's owner (the enclosing body, or the environment).
//! Each row counts drops through a raw pointer, so a missed or doubled drop changes the status.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

/// A value whose drop counts itself at `p`.
struct Counted {
    p: *mut u8,
    v: u8,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            *self.p = *self.p + 1;
        }
    }
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The site's shape: `r` captured by `&mut` in an `FnOnce(&mut u8)` closure handed to a fn
/// (`init_with_mut`), assigned there. The old value drops at the assignment (1), the new one
/// at `r`'s scope end (2): `r.v + b` = 30 + 5, plus 10 per drop: 55. With no drop at the
/// assignment 45; refused before the fix.
#[test]
fn an_assignment_through_a_mut_capture_drops_the_old_value() {
    let got = run("
fn with_mut<F: FnOnce(&mut u8)>(f: F) -> u8 {
    let mut b = 2u8;
    f(&mut b);
    b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let got;
    {
        let mut r = Counted { p, v: 1 };
        let b = with_mut(|buf: &mut u8| {
            *buf = 5;
            r = Counted { p, v: 30 };
        });
        got = r.v + b;
    }
    (got + 10 * drops) as i32
}
");
    println!("ASSIGN THROUGH A MUT CAPTURE: {got:?}");
    assert_eq!(got, Ok(55), "old value dropped at the assignment, the new one at the scope's end");
}

/// The twin: `r` captured BY VALUE (`move`) in an `FnMut` body called twice: the environment
/// owns `r` and the body borrows it. Each call drops the old value (2), the environment's drop
/// at `twice`'s end drops the last (3): `11 + 21` = 32, plus 10 per drop: 62.
#[test]
fn an_assignment_to_a_by_value_capture_of_an_fn_mut_body_drops_the_old_value() {
    let got = run("
fn twice<F: FnMut() -> u8>(mut f: F) -> u8 {
    let a = f();
    let b = f();
    a + b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let mut r = Counted { p, v: 1 };
    let s = twice(move || {
        let old = r.v;
        r = Counted { p, v: old + 10 };
        r.v
    });
    (s + 10 * drops) as i32
}
");
    println!("ASSIGN TO A BY-VALUE CAPTURE OF AN FnMut BODY: {got:?}");
    assert_eq!(got, Ok(62), "two assignments drop two old values; the environment drops the last");
}

/// The control: the same assignment in the body that owns `r` (passed before the fix): one
/// drop at the assignment, one at the end: 7 + 20 = 27.
#[test]
fn an_assignment_to_an_owned_binding_drops_the_old_value() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u8;
    let p = &mut drops as *mut u8;
    let got;
    {
        let mut r = Counted { p, v: 1 };
        r = Counted { p, v: 7 };
        got = r.v;
    }
    (got + 10 * drops) as i32
}
");
    println!("ASSIGN TO AN OWNED BINDING: {got:?}");
    assert_eq!(got, Ok(27), "one drop at the assignment, one at the scope's end");
}

/// Regression for the tempo `AA2dPool::descendant_txs` SIGSEGV: a non-`move` closure reading
/// `id.seq` through a borrowed parameter `id: &'b TxId` and returned from the function. Under
/// RFC 2229 (`BuildEnv = Places`), the closure captures `*id` (`&'b TxId`), NOT `&id` (a
/// pointer to the parameter's dead stack slot in `make_matcher`). After `clobber_stack`
/// overwrites the old stack frame, invoking the closure compares the live `[u8; 20]` arrays
/// without dereferencing garbage.
#[test]
fn returned_closure_capturing_through_shared_ref_does_not_dangle() {
    let got = run("
struct Seq {
    addr: [u8; 20],
    key: u64,
}

impl Seq {
    fn same(&self, other: &Seq) -> bool {
        let mut i = 0usize;
        while i < 20 {
            if self.addr[i] != other.addr[i] {
                return false;
            }
            i = i + 1;
        }
        self.key == other.key
    }
}

struct TxId {
    seq: Seq,
    nonce: u64,
}

#[inline(never)]
fn make_matcher<'a>(id: &'a TxId) -> impl Fn(&TxId) -> bool + 'a {
    |other: &TxId| id.seq.same(&other.seq)
}

#[inline(never)]
fn clobber_stack(seed: u64) -> u64 {
    let mut buf = [seed; 64];
    let mut i = 0usize;
    let mut acc = 0u64;
    while i < 64 {
        buf[i] = seed ^ (i as u64);
        acc = acc ^ buf[i];
        i = i + 1;
    }
    acc
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = TxId { seq: Seq { addr: [7u8; 20], key: 42 }, nonce: 1 };
    let b = TxId { seq: Seq { addr: [7u8; 20], key: 42 }, nonce: 2 };
    let c = TxId { seq: Seq { addr: [9u8; 20], key: 42 }, nonce: 3 };
    let pred = make_matcher(&a);
    let _ = clobber_stack(0xdead_beef_cafe_babeu64);
    let m1 = if pred(&b) { 10 } else { 0 };
    let m2 = if pred(&c) { 1 } else { 0 };
    m1 + m2
}
");
    println!("RETURNED CLOSURE THROUGH SHARED REF: {got:?}");
    assert_eq!(got, Ok(10), "captured `*id` survives return and stack clobber");
}

/// Companion: a returned non-`move` closure reading through a WIDE shared reference (`&'a [u8]`)
/// and a closure capturing the unsized tail field of a DST struct (`core::array::iter::PolymorphicIter::next`).
#[test]
fn closure_capturing_through_wide_ref_and_dst_tail_runs() {
    let got = run("
struct Dst<T: PointeeSized> {
    head: u8,
    tail: T,
}

#[inline(never)]
fn make_reader<'a>(s: &'a [u8]) -> impl Fn(usize) -> u8 + 'a {
    |i: usize| s[i]
}

#[inline(never)]
fn clobber_stack(seed: u64) -> u64 {
    let mut buf = [seed; 64];
    let mut i = 0usize;
    let mut acc = 0u64;
    while i < 64 {
        buf[i] = seed ^ (i as u64);
        acc = acc ^ buf[i];
        i = i + 1;
    }
    acc
}

fn read_dst_tail(d: &mut Dst<[u8]>) -> u8 {
    let f = |i: usize| d.tail[i];
    f(1) + f(2)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let data = [3u8, 10u8, 20u8];
    let reader = make_reader(&data);
    let _ = clobber_stack(0xdead_beef_cafe_babeu64);
    let a = reader(0);
    let mut dst = Dst { head: 1, tail: [5u8, 11u8, 22u8] };
    let b = read_dst_tail(&mut dst);
    (a + b) as i32
}
");
    println!("WIDE REF AND DST TAIL CAPTURE: {got:?}");
    assert_eq!(got, Ok(36), "3 + (11 + 22) = 36");
}

/// Companion: a returned non-`move` `FnMut` closure calling a `&mut self` method through a
/// borrowed `&'a mut Counter` parameter (auto-deref + reborrow in `push_deref_chain` ->
/// `PlaceDerefs` -> `env_use`).
#[test]
fn returned_closure_calling_method_through_mut_ref_does_not_dangle() {
    let got = run("
struct Counter {
    n: u8,
}

impl Counter {
    fn bump(&mut self, delta: u8) -> u8 {
        self.n = self.n + delta;
        self.n
    }
}

#[inline(never)]
fn make_bumper<'a>(c: &'a mut Counter) -> impl FnMut(u8) -> u8 + 'a {
    |d: u8| c.bump(d)
}

#[inline(never)]
fn clobber_stack(seed: u64) -> u64 {
    let mut buf = [seed; 64];
    let mut i = 0usize;
    let mut acc = 0u64;
    while i < 64 {
        buf[i] = seed ^ (i as u64);
        acc = acc ^ buf[i];
        i = i + 1;
    }
    acc
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut c = Counter { n: 5 };
    let mut f = make_bumper(&mut c);
    let _ = clobber_stack(0xdead_beef_cafe_babeu64);
    let a = f(3);
    let b = f(4);
    (a + b) as i32
}
");
    println!("RETURNED CLOSURE THROUGH MUT REF METHOD: {got:?}");
    assert_eq!(got, Ok(20), "8 + 12 = 20");
}

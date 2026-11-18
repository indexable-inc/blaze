//! A union declared inside a fn body, generic over parameters of its own that mirror the
//! enclosing fn's, BUILT AND RUN. The form is the pinned std's `panicking::catch_unwind`
//! (library/std/src/panicking.rs): `union Data<F, R> { f: ManuallyDrop<F>, r: ManuallyDrop<R>, .. }`
//! and `unsafe fn do_call<F: FnOnce() -> R, R>(data: *mut Data<F, R>)` are items of one body, the
//! body writes `Data { f: .. }` and passes `&raw mut data` to the local fn, which reads a field.
//! std-hello's run stopped at it (2026-11-17, g111548): "a union in a block that a macro call
//! wrote or that is in a local item's body".
//!
//! The rules, the Reference:
//! - "Statements", "Item declarations": an item in a block is scoped to the block; "there is
//!   no implicit capture of the containing function's generic parameters", so the union
//!   declares its own `<F, R>`.
//! - "Unions": "all fields share common storage"; "reads of union fields are unsafe" (E0133
//!   outside `unsafe`); a union field's type is `Copy` or `ManuallyDrop<_>`, and "unions
//!   never drop their fields": a union has no drop glue.
//!
//! The programs are `#![no_core]` (no toolchain); the lang items are the pinned core's
//! (marker.rs, ops/drop.rs, ptr/mod.rs `drop_in_place`, mem/manually_drop.rs).

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

/// No arithmetic below: overflow checks off.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

const HEAD: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[lang = \"drop_in_place\"]
unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T) {
    unsafe { drop_in_place(to_drop) }
}

#[lang = \"manually_drop\"]
#[repr(transparent)]
struct ManuallyDrop<T> {
    value: T,
}

impl<T> ManuallyDrop<T> {
    const fn new(value: T) -> ManuallyDrop<T> {
        ManuallyDrop { value }
    }
}

/// Four bytes, `Copy`: a `u32`'s layout read byte by byte.
struct Four {
    a: u8,
    b: u8,
    c: u8,
    d: u8,
}

impl Copy for Four {}

/// Writes 9 where it points when dropped.
struct Loud {
    at: *mut u32,
}

impl Drop for Loud {
    fn drop(&mut self) {
        unsafe { *self.at = 9 }
    }
}
";

/// `catch_unwind`'s shape: a local generic union written through `f` and read through `r` by
/// a local fn beside it, through a raw pointer. 42 written as a `u32`, read as `Four`: its
/// first byte (little-endian) is 42.
const PUN: &str = "
unsafe fn pun<F: Copy, R: Copy>(f: F) -> R {
    union Data<F: Copy, R: Copy> {
        f: F,
        r: R,
    }
    let mut data = Data { f };
    return unsafe { read_r(&raw mut data) };

    unsafe fn read_r<F: Copy, R: Copy>(data: *mut Data<F, R>) -> R {
        unsafe { (*data).r }
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let four: Four = unsafe { pun::<u32, Four>(42) };
    four.a as i32
}
";

/// A local union holding a `Loud` in a `ManuallyDrop` field goes out of scope: nothing is
/// dropped, so `n` stays 7.
const KEPT: &str = "
fn kept(at: *mut u32) {
    union Hold<T> {
        l: ManuallyDrop<T>,
        n: u32,
    }
    let _h = Hold { l: ManuallyDrop::new(Loud { at }) };
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: u32 = 7;
    kept(&raw mut n);
    n as i32
}
";

/// Control for [`KEPT`]: the same `Loud` held plainly is dropped, so `n` is 9.
const DROPPED: &str = "
fn dropped(at: *mut u32) {
    let _l = Loud { at };
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: u32 = 7;
    dropped(&raw mut n);
    n as i32
}
";

/// Builds and runs `HEAD` + `tail`: the status, or the refusal on one line.
fn run(tail: &str) -> Result<u8, String> {
    let text = [HEAD, tail].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(600).map_or(said.clone(), |(i, _)| said[..i].to_string())
    })
}

/// Prints "LOCAL UNION PUN: Ok(42)".
#[test]
fn a_local_generic_union_puns_its_bytes() {
    let got = run(PUN);
    println!("LOCAL UNION PUN: {got:?}");
    assert_eq!(got, Ok(42));
}

/// Prints "LOCAL UNION KEPT: Ok(7); PLAIN DROPPED: Ok(9)".
#[test]
fn a_local_union_drops_no_field() {
    let kept = run(KEPT);
    let dropped = run(DROPPED);
    println!("LOCAL UNION KEPT: {kept:?}; PLAIN DROPPED: {dropped:?}");
    assert_eq!(dropped, Ok(9), "control: Loud's drop runs when held plainly");
    assert_eq!(kept, Ok(7), "a union never drops its fields");
}

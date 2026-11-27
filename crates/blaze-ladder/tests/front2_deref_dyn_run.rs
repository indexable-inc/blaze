//! Dynamic vtable dispatch in `Walk::deref_call` (blaze-build walk.rs), BUILT AND RUN natively on
//! front2: dereferencing `&dyn Trait` / `&mut dyn Trait` where `Trait` is `Deref` / `DerefMut` or
//! has `Deref` / `DerefMut` as a supertrait (unblocking `derive-where`'s `trait TraitImpl:
//! Deref<Target = Trait>` called via `***trait_`).
//!
//! Covers:
//! - supertrait `trait Sub: Deref<Target = u32>` via `**r` and `***rr` (and multiple concrete
//!   types through one `&dyn Sub` function),
//! - supertrait `trait SubMut: DerefMut<Target = u32>` mutating via `**r = 99` on `&mut dyn
//!   SubMut`,
//! - unsized target `trait SubStr: Deref<Target = str>` returning wide `&str` through `&dyn
//!   SubStr` and checking `.len()` and bytes (both explicit `&**r` and auto-deref method calls),
//! - direct `&dyn Deref<Target = u32>` and `&mut dyn DerefMut<Target = u32>`,
//! - auto-deref method call on `&dyn Sub` / `&dyn SubStr`.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/twin-prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[lang = \"deref\"]
pub trait Deref {
    #[lang = \"deref_target\"]
    type Target: ?Sized;
    fn deref(&self) -> &Self::Target;
}

#[lang = \"deref_mut\"]
pub trait DerefMut: Deref {
    fn deref_mut(&mut self) -> &mut Self::Target;
}

trait Sub: Deref<Target = u32> {
    fn tag(&self) -> u32;
}

struct A {
    val: u32,
}

impl Deref for A {
    type Target = u32;
    fn deref(&self) -> &u32 {
        &self.val
    }
}

impl DerefMut for A {
    fn deref_mut(&mut self) -> &mut u32 {
        &mut self.val
    }
}

impl Sub for A {
    fn tag(&self) -> u32 {
        1
    }
}

struct B {
    pad: u64,
    val: u32,
}

impl Deref for B {
    type Target = u32;
    fn deref(&self) -> &u32 {
        &self.val
    }
}

impl Sub for B {
    fn tag(&self) -> u32 {
        2
    }
}

trait SubMut: DerefMut<Target = u32> {
    fn kind(&self) -> u32;
}

impl SubMut for A {
    fn kind(&self) -> u32 {
        10
    }
}

trait SubStr: Deref<Target = str> {
    fn extra(&self) -> usize;
}

struct HeldStr {
    text: &'static str,
    n: usize,
}

impl Deref for HeldStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.text
    }
}

impl SubStr for HeldStr {
    fn extra(&self) -> usize {
        self.n
    }
}

trait SubSlice: Deref<Target = [u8]> {}

struct HeldBytes {
    bytes: &'static [u8],
}

impl Deref for HeldBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.bytes
    }
}

impl SubSlice for HeldBytes {}

trait InnerDyn {
    fn score(&self) -> u32;
}

struct Leaf(u32);

impl InnerDyn for Leaf {
    fn score(&self) -> u32 {
        self.0
    }
}

trait SubDyn: Deref<Target = dyn InnerDyn> {}

struct HoldDyn {
    inner: &'static dyn InnerDyn,
}

impl Deref for HoldDyn {
    type Target = dyn InnerDyn;
    fn deref(&self) -> &dyn InnerDyn {
        self.inner
    }
}

impl SubDyn for HoldDyn {}

trait SubPair: Deref<Target = Pair> {}

struct Pair {
    lo: u32,
    hi: u32,
}

impl Pair {
    fn sum(&self) -> u32 {
        self.lo + self.hi
    }
}

struct WrapPair {
    pair: Pair,
}

impl Deref for WrapPair {
    type Target = Pair;
    fn deref(&self) -> &Pair {
        &self.pair
    }
}

impl SubPair for WrapPair {}

fn str_bytes(s: &str) -> &[u8] {
    unsafe { transmute(s) }
}

fn str_len(s: &str) -> usize {
    let (_, len): (*const u8, usize) = unsafe { transmute(s) };
    len
}

fn read_sub(r: &dyn Sub) -> u32 {
    **r + r.tag()
}

fn read_sub_ref(rr: &&dyn Sub) -> u32 {
    ***rr
}

fn write_sub_mut(r: &mut dyn SubMut, v: u32) {
    **r = v;
}

fn read_direct(r: &dyn Deref<Target = u32>) -> u32 {
    **r
}

fn read_direct_ref(rr: &&dyn Deref<Target = u32>) -> u32 {
    ***rr
}

fn write_direct_mut(r: &mut dyn DerefMut<Target = u32>, v: u32) {
    **r = v;
}

fn check_sub_str(r: &dyn SubStr) -> usize {
    let s: &str = &**r;
    let bytes = str_bytes(s);
    str_len(s) + (bytes[0] as usize) + (bytes[3] as usize) + r.extra()
}

fn check_sub_slice(r: &dyn SubSlice) -> u8 {
    (*r)[0] + (*r)[2]
}

fn check_sub_dyn(r: &dyn SubDyn) -> u32 {
    (**r).score()
}

fn auto_deref_method(r: &dyn SubPair) -> u32 {
    r.sum()
}
";

fn run(main: &str) -> u8 {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("run_with failed: {e}"))
}

/// Supertrait `trait Sub: Deref<Target = u32>` called via `**r` and `***rr` on `&dyn Sub` for
/// two different concrete layouts (`A` at offset 0, `B` after an 8-byte pad):
/// `read_sub(&a)` = 10 + 1 = 11, `read_sub(&b)` = 20 + 2 = 22, `read_sub_ref(&rb)` = 20.
/// Total: 11 + 22 + 20 = 53.
#[test]
fn supertrait_deref_on_dyn_dispatches_via_vtable() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = A { val: 10 };
    let b = B { pad: 999, val: 20 };
    let ra: &dyn Sub = &a;
    let rb: &dyn Sub = &b;
    (read_sub(ra) + read_sub(rb) + read_sub_ref(&rb)) as i32
}
");
    assert_eq!(got, 53, "supertrait Deref on &dyn Sub via **r and ***rr");
}

/// Supertrait `trait SubMut: DerefMut<Target = u32>` mutating through `&mut dyn SubMut` (`**r =
/// 99`) and reading back both via `a.val` and via `**r`: 99 + 10 = 109.
#[test]
fn supertrait_deref_mut_on_dyn_mutates_target() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut a = A { val: 5 };
    let k = {
        let r: &mut dyn SubMut = &mut a;
        write_sub_mut(r, 99);
        r.kind()
    };
    (a.val + k) as i32
}
");
    assert_eq!(got, 109, "supertrait DerefMut on &mut dyn SubMut via **r = 99");
}

/// Unsized targets through `&dyn SubStr` (`Target = str`), `&dyn SubSlice` (`Target = [u8]`),
/// and `&dyn SubDyn` (`Target = dyn InnerDyn`): wide pointers `&str` / `&[u8]` / `&dyn InnerDyn`
/// returned from the vtable slot, metadata and data preserved.
/// `\"Rust\"`: len 4, bytes[0] = b'R' (82), bytes[3] = b't' (116), extra = 3 -> 205;
/// `[7, 1, 9]`: 7 + 9 = 16; `Leaf(19)`: 19; (205 - 180) + 16 + 19 = 60.
#[test]
fn unsized_target_deref_on_dyn_returns_wide_ref() {
    let got = run("
static LEAF: Leaf = Leaf(19);
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let hs = HeldStr { text: \"Rust\", n: 3 };
    let hb = HeldBytes { bytes: &[7u8, 1u8, 9u8] };
    let hd = HoldDyn { inner: &LEAF };
    let s_score = check_sub_str(&hs);
    let b_score = check_sub_slice(&hb) as usize;
    let d_score = check_sub_dyn(&hd) as usize;
    ((s_score - 180) + b_score + d_score) as i32
}
");
    assert_eq!(got, 60, "unsized Target = str, [u8], and dyn InnerDyn through &dyn");
}

/// Direct `&dyn Deref<Target = u32>` (`**r` and `***rr`), `&mut dyn DerefMut<Target = u32>`
/// (`**r = 42`), and auto-deref method call `r.sum()` on `&dyn SubPair` (13 + 28 = 41).
/// Total: 12 + 12 + 42 + 41 = 107.
#[test]
fn direct_dyn_deref_and_auto_deref_method_dispatch_via_vtable() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut a = A { val: 12 };
    let r: &dyn Deref<Target = u32> = &a;
    let direct_read = read_direct(r) + read_direct_ref(&r);
    write_direct_mut(&mut a, 42);
    let wp = WrapPair { pair: Pair { lo: 13, hi: 28 } };
    let method_val = auto_deref_method(&wp);
    (direct_read + a.val + method_val) as i32
}
");
    assert_eq!(got, 107, "direct &dyn Deref / &mut dyn DerefMut and auto-deref method call");
}

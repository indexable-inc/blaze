//! Core's real `Layout::from_size_align_unchecked` shape, one difference at a time (RF12 piece
//! 5). std hello's trap read a STACK ADDRESS as `AlignmentEnum`'s discriminant (x9 =
//! 0x16b4c6828), while small_main.rs's (c2), the same transmute in a plain `const fn`, returns
//! 250. Each row below is (c2) with ONE thing changed toward core's text (layout.rs:127-144,
//! alignment.rs), and (r7) is all of them at once:
//!
//! - (r1) `transmute(align)` with its type arguments INFERRED (core writes
//!   `mem::transmute(align)`, `Dst` taken from the field `align: Alignment`).
//! - (r2) the call through a re-export, `mod mem { pub use crate::transmute; }`, as core's
//!   `mem::transmute` is `pub use crate::intrinsics::transmute`.
//! - (r3) the newtype with a NAMED field, `struct W { inner: A64 }`, as core's
//!   `Alignment { _inner_repr_trick: AlignmentEnum }`.
//! - (r4) `#[track_caller]` on the fn: its hidden last parameter, a `&Location` (blaze-build
//!   walk/track_caller.rs), beside the result returned through its pointer (`L` is an
//!   aggregate, walk.rs `rust_pass`: `Pass::Ref`).
//! - (r5) a `const unsafe fn` whose struct literal is the tail of an `unsafe { }` block.
//! - (r6) a body-declared `const fn check(size: usize, align: usize)` called before the literal,
//!   its parameters named as the outer fn's (`assert_unsafe_precondition!`'s
//!   `precondition_check`, with its `#[rustc_no_mir_inline] #[inline] #[rustc_nounwind]`).
//! - (r7) all of them together: the real shape.
//!
//! Every program returns 250 when every `n` in 0..63 passed (2^63 is refused by `valid`, as
//! core's `Layout::from_size_align` refuses it), else 1 + n for a wrong size, 64 + n for a
//! wrong alignment value read back through the enum's copy, 128 + n for `valid` refusing.
//! Every row is walked over names records and run whole, and every row's line is printed
//! before any is asserted: "real shape <row>: walk <counts or refusal>; status <n or refusal>".

use super::{MemoryStore, PASSED, alloc, fixture, run_whole, walk_into};

/// The lang items (`panic_location` too, for (r4) and (r7)'s calls of a tracked fn from an
/// untracked `main`, which make a `Location` row), `Copy` for the loop's types, `transmute` as
/// the pinned core declares it, and `AlignmentEnum`'s 64 variants as `A64` with its copy.
macro_rules! head {
    () => {
        "#![feature(no_core, lang_items, rustc_attrs, intrinsics)]
#![no_core]
#![no_main]
#![allow(internal_features, dead_code, unused_unsafe)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for usize {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[rustc_nounwind]
#[rustc_intrinsic]
pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;

#[repr(usize)]
enum A64 {
    S0 = 1 << 0,
    S1 = 1 << 1,
    S2 = 1 << 2,
    S3 = 1 << 3,
    S4 = 1 << 4,
    S5 = 1 << 5,
    S6 = 1 << 6,
    S7 = 1 << 7,
    S8 = 1 << 8,
    S9 = 1 << 9,
    S10 = 1 << 10,
    S11 = 1 << 11,
    S12 = 1 << 12,
    S13 = 1 << 13,
    S14 = 1 << 14,
    S15 = 1 << 15,
    S16 = 1 << 16,
    S17 = 1 << 17,
    S18 = 1 << 18,
    S19 = 1 << 19,
    S20 = 1 << 20,
    S21 = 1 << 21,
    S22 = 1 << 22,
    S23 = 1 << 23,
    S24 = 1 << 24,
    S25 = 1 << 25,
    S26 = 1 << 26,
    S27 = 1 << 27,
    S28 = 1 << 28,
    S29 = 1 << 29,
    S30 = 1 << 30,
    S31 = 1 << 31,
    S32 = 1 << 32,
    S33 = 1 << 33,
    S34 = 1 << 34,
    S35 = 1 << 35,
    S36 = 1 << 36,
    S37 = 1 << 37,
    S38 = 1 << 38,
    S39 = 1 << 39,
    S40 = 1 << 40,
    S41 = 1 << 41,
    S42 = 1 << 42,
    S43 = 1 << 43,
    S44 = 1 << 44,
    S45 = 1 << 45,
    S46 = 1 << 46,
    S47 = 1 << 47,
    S48 = 1 << 48,
    S49 = 1 << 49,
    S50 = 1 << 50,
    S51 = 1 << 51,
    S52 = 1 << 52,
    S53 = 1 << 53,
    S54 = 1 << 54,
    S55 = 1 << 55,
    S56 = 1 << 56,
    S57 = 1 << 57,
    S58 = 1 << 58,
    S59 = 1 << 59,
    S60 = 1 << 60,
    S61 = 1 << 61,
    S62 = 1 << 62,
    S63 = 1 << 63,
}

#[inline(never)]
fn copy(a: A64) -> A64 {
    a
}

#[inline(never)]
fn valid(size: usize, align: usize) -> bool {
    if align == 0 {
        return false;
    }
    if align & (align - 1) != 0 {
        return false;
    }
    size <= 9223372036854775807 - (align - 1)
}
"
    };
}

/// `L` and its by-value pass, after the row's `W`.
macro_rules! layout {
    () => {
        "
struct L {
    size: usize,
    align: W,
}

#[inline(never)]
fn pass(l: L) -> L {
    l
}
"
    };
}

/// (c2)'s newtype: a tuple struct.
macro_rules! tuple_w {
    () => {
        "
#[repr(transparent)]
struct W(A64);
"
    };
}

/// Core's `Alignment`: a named field.
macro_rules! named_w {
    () => {
        "
#[repr(transparent)]
struct W {
    inner: A64,
}
"
    };
}

/// `main`: `make` is the call that builds an `L` from `16` and `want`, `field` the newtype's
/// field (`0` or `inner`).
macro_rules! main_of {
    ($make:literal, $field:literal) => {
        concat!(
            "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: usize = 0;
    while n < 63 {
        let want: usize = 1usize << n;
        if !valid(16, want) {
            return (128 + n) as i32;
        }
        let l = pass(",
            $make,
            ");
        if l.size != 16 {
            return (1 + n) as i32;
        }
        let a = copy(l.align.",
            $field,
            ");
        if a as usize != want {
            return (64 + n) as i32;
        }
        n += 1;
    }
    250
}
"
        )
    };
}

/// (r1): `Dst` and `Src` inferred.
const R1: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    "
const fn make_const(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute(align) } }
}
",
    main_of!("make_const(16, want)", "0")
);

/// (r2): through `mem`'s re-export.
const R2: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    "
mod mem {
    pub use crate::transmute;
}

const fn make_const(size: usize, align: usize) -> L {
    L { size, align: unsafe { mem::transmute::<usize, W>(align) } }
}
",
    main_of!("make_const(16, want)", "0")
);

/// (r3): the named-field newtype.
const R3: &str = concat!(
    head!(),
    named_w!(),
    layout!(),
    "
const fn make_const(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute::<usize, W>(align) } }
}
",
    main_of!("make_const(16, want)", "inner")
);

/// (r4): `#[track_caller]`.
const R4: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    "
#[track_caller]
const fn make_const(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute::<usize, W>(align) } }
}
",
    main_of!("make_const(16, want)", "0")
);

/// (r5): a `const unsafe fn`, the literal the tail of an `unsafe` block.
const R5: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    "
const unsafe fn make_const(size: usize, align: usize) -> L {
    unsafe { L { size, align: transmute::<usize, W>(align) } }
}
",
    main_of!("unsafe { make_const(16, want) }", "0")
);

/// (r6): a body-declared check, called first, its parameters named as the outer fn's.
const R6: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    "
const fn make_const(size: usize, align: usize) -> L {
    #[rustc_no_mir_inline]
    #[inline]
    #[rustc_nounwind]
    const fn check(size: usize, align: usize) {
        let low = align & (align - 1);
        let _sum = size + low;
    }
    check(size, align);
    L { size, align: unsafe { transmute::<usize, W>(align) } }
}
",
    main_of!("make_const(16, want)", "0")
);

/// (r7): all six at once, as core writes `from_size_align_unchecked`.
const R7: &str = concat!(
    head!(),
    named_w!(),
    layout!(),
    "
mod mem {
    pub use crate::transmute;
}

#[inline]
#[track_caller]
const unsafe fn make_const(size: usize, align: usize) -> L {
    #[rustc_no_mir_inline]
    #[inline]
    #[rustc_nounwind]
    #[track_caller]
    const fn check(size: usize, align: usize) {
        let low = align & (align - 1);
        let _sum = size + low;
    }
    check(size, align);
    unsafe { L { size, align: mem::transmute(align) } }
}
",
    main_of!("unsafe { make_const(16, want) }", "inner")
);

// THE REVERSE DIRECTION (piece 5, second round): rows (r1)..(r7) all return 250, so `align`
// is already a stack address when core's `from_size_align_unchecked` receives it; its callers
// make that `usize` back out of an `Alignment` (alloc/layout.rs:187-189, mem/alignment.rs:
// 205-235): `Layout::align(&self) -> usize { self.align.as_usize() }`, `Alignment::as_usize
// (self) { self.as_nonzero_usize().get() }`, `as_nonzero_usize(self) -> NonZero<usize> {
// transmute::<Alignment, NonZero<usize>>(self) }`, `NonZero::get(self) -> T {
// transmute_unchecked(self) }`. `NonZero<usize>` is `#[repr(transparent)] NonZero(
// NonZeroUsizeInner)`, the inner a `#[repr(transparent)]` newtype over the pattern type `usize
// is 1..` (num/niche_types.rs): here a `#[rustc_layout_scalar_valid_range_start(1)]` newtype
// stands for it (a pattern type needs core's `pattern_type!`). Each row's `main` returns 250,
// else 1 + n for a wrong size, 64 + n for its first read wrong, 128 + n for its second, 200 for
// `valid` refusing.

/// `NZ` (core's `NonZero<usize>`: transparent over a transparent valid-range newtype over
/// `usize`), `as_nz` (the transmute FROM the by-pointer aggregate `W` TO `NZ`), `NZ::get` of
/// body `$get`, `W::as_usize` and `L::align(&self)` as core writes them, and `Copy` for the
/// enum and the newtype (`as_usize(self)` of a field behind `&self` copies it out).
macro_rules! chain {
    ($get:literal) => {
        concat!(
            "
impl Copy for A64 {}
impl Copy for W {}

#[rustc_layout_scalar_valid_range_start(1)]
#[repr(transparent)]
struct Inner(usize);

#[repr(transparent)]
struct NZ(Inner);

#[inline(never)]
fn as_nz(a: W) -> NZ {
    unsafe { transmute::<W, NZ>(a) }
}

impl NZ {
    #[inline(never)]
    fn get(self) -> usize {
        ",
            $get,
            "
    }
}

impl W {
    #[inline(never)]
    fn as_usize(self) -> usize {
        as_nz(self).get()
    }
}

impl L {
    #[inline(never)]
    fn align(&self) -> usize {
        self.align.as_usize()
    }
}
"
        )
    };
}

/// `main` with two reads of the alignment back as a `usize`, `first` and `second`, after `make`
/// built `l`.
macro_rules! main_two {
    ($make:literal, $first:literal, $second:literal) => {
        concat!(
            "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: usize = 0;
    while n < 63 {
        let want: usize = 1usize << n;
        if !valid(16, want) {
            return 200;
        }
        let l = pass(",
            $make,
            ");
        if l.size != 16 {
            return (1 + n) as i32;
        }
        let first: usize = ",
            $first,
            ";
        if first != want {
            return (64 + n) as i32;
        }
        let second: usize = ",
            $second,
            ";
        if second != want {
            return (128 + n) as i32;
        }
        n += 1;
    }
    250
}
"
        )
    };
}

/// (c2)'s `make_const`.
macro_rules! plain_make {
    () => {
        "
const fn make_const(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute::<usize, W>(align) } }
}
"
    };
}

/// (r8): the transmute `W -> NZ` by value, then `get` as `self.0.0`; first of `l`'s own
/// alignment, then of a fresh one's.
const R8: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    chain!("self.0.0"),
    plain_make!(),
    main_two!("make_const(16, want)", "as_nz(l.align).get()", "as_nz(pass(make_const(16, want)).align).get()")
);

/// (r9): through `&self` on `L` (`l.align()`), then the round trip `make_const(l.size,
/// l.align())` read back the same way.
const R9: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    chain!("self.0.0"),
    plain_make!(),
    main_two!("make_const(16, want)", "l.align()", "pass(make_const(l.size, l.align())).align()")
);

/// (r10): `as usize` of the enum held in `L`, behind a reference and by value (a Rust-ABI
/// aggregate argument is passed by pointer either way).
const R10: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    "
impl Copy for A64 {}

#[inline(never)]
fn cast_by_ref(l: &L) -> usize {
    l.align.0 as usize
}

#[inline(never)]
fn cast_by_value(l: L) -> usize {
    l.align.0 as usize
}
",
    plain_make!(),
    main_two!("make_const(16, want)", "cast_by_ref(&l)", "cast_by_value(l)")
);

/// (r11): (r9) with (r7)'s `make_const`, core's real callee (`#[track_caller] const unsafe
/// fn`, the named field, `mem::transmute` inferred, the body-declared check): the whole real
/// chain in one program.
const R11: &str = concat!(
    head!(),
    named_w!(),
    layout!(),
    chain!("self.0.0"),
    "
mod mem {
    pub use crate::transmute;
}

#[inline]
#[track_caller]
const unsafe fn make_const(size: usize, align: usize) -> L {
    #[rustc_no_mir_inline]
    #[inline]
    #[rustc_nounwind]
    #[track_caller]
    const fn check(size: usize, align: usize) {
        let low = align & (align - 1);
        let _sum = size + low;
    }
    check(size, align);
    unsafe { L { size, align: mem::transmute(align) } }
}
",
    main_two!("unsafe { make_const(16, want) }", "l.align()", "pass(unsafe { make_const(l.size, l.align()) }).align()")
);

/// (r12): (r9) with `get` as core writes it, a transmute `NZ -> usize` by value (core's
/// `transmute_unchecked`, the same lowering arm: intrinsics.rs `W::Transmute |
/// W::TransmuteUnchecked`).
const R12: &str = concat!(
    head!(),
    tuple_w!(),
    layout!(),
    chain!("unsafe { transmute::<NZ, usize>(self) }"),
    plain_make!(),
    main_two!("make_const(16, want)", "l.align()", "pass(make_const(l.size, l.align())).align()")
);

const ROWS: [(&str, &str); 12] = [
    ("(r1) inferred transmute", R1),
    ("(r2) mem::transmute re-export", R2),
    ("(r3) named-field newtype", R3),
    ("(r4) #[track_caller]", R4),
    ("(r5) const unsafe fn, unsafe-block tail", R5),
    ("(r6) body-declared const fn check", R6),
    ("(r7) all together, the real shape", R7),
    ("(r8) transmute W -> NZ, get as self.0.0", R8),
    ("(r9) &self align() and the round trip", R9),
    ("(r10) as usize of the enum in L, by ref and by value", R10),
    ("(r11) r9 with the real callee", R11),
    ("(r12) r9 with get by transmute NZ -> usize", R12),
];

/// Every row walked and run, every line printed, then every row asserted: 250 is every `n`
/// passed, 1 + n a wrong size; (r1)..(r7): 64 + n a wrong alignment value, 128 + n `valid`
/// refusing; (r8)..(r12): 64 + n the first read wrong, 128 + n the second, 200 `valid`.
#[test]
fn real_shape_one_difference_at_a_time() {
    let said = ROWS.map(|(name, text)| {
        let f = fixture(text);
        let mut store = MemoryStore::new_in(alloc());
        let walked = walk_into(&f, &mut store);
        let ran = run_whole(text);
        match (&walked, &ran) {
            (Ok(counts), Ok(status)) => println!("real shape {name}: walk {counts}; status {status}"),
            (Ok(counts), Err(e)) => println!("real shape {name}: walk {counts}; status REFUSED: {e}"),
            (Err(e), Ok(status)) => println!("real shape {name}: walk REFUSED: {e}; status {status}"),
            (Err(e), Err(r)) => println!("real shape {name}: walk REFUSED: {e}; status REFUSED: {r}"),
        }
        (name, text, walked.is_ok(), ran)
    });
    for (name, text, walked, ran) in said {
        assert!(walked, "{name}: the lazy path refused (its line above)");
        match ran {
            Ok(status) => assert_eq!(status, PASSED, "{name}: the statuses are in this test's doc\n{text}"),
            Err(e) => panic!("{name}: the whole run refused: {e}"),
        }
    }
}

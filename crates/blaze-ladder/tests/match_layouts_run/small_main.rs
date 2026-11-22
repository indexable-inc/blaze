//! Row (c) made runnable (RF12 piece 2): all 64 of `AlignmentEnum`'s sparse variants, with a
//! SMALL `main` (a loop over `n`, not 64 unrolled checks: row (c) above is refused for its size,
//! a branch past the baseline tier's reach). Two programs, each walked over names records (the
//! lazy path) and run whole, each returning 250 when every `n` passed:
//!
//! (c1) RUN-TIME TRANSMUTE: for n in 0..64, `1usize << n` computed at run time, made into
//! `L { size: 16, align: transmute::<usize, W>(1 << n) }` by an `#[inline(never)] unsafe fn
//! make`, passed by value through `pass(l: L) -> L` (the struct's copy: blaze-build walk.rs
//! `copy` switches on the enum's discriminant, the switch std hello trapped in), then its enum
//! through `copy(a: A64) -> A64` (the enum's own copy), and compared `as usize` with
//! `1 << n`. Returns 1 + n for the first `n` whose size or value came back wrong. Isolates: the
//! 64-case switch over sparse values up to 2^63 (cases past 4095 compared through `mov_imm`
//! into x10, blaze-emit pass.rs `Op::Switch`), the copy through the `repr(transparent)`
//! newtype inside a `Layout`-shaped struct, and the discriminants the layout reads (front2
//! heads.rs `discriminants`) agreeing with the ones `transmute` writes (the bits). A trap here
//! (status 133 under `blaze run`, the run whole refused naming `brk #0xff02`) is h1/h2/h3 below
//! confirmed in a 30-line program; 250 rules them out for this shape.
//!
//! (c2) CONST-FN TRANSMUTE after a check: as core's `Layout::from_size_align` runs it, an
//! `#[inline(never)] fn valid(size, align) -> bool` (`align` a power of two, and `size` at
//! most `isize::MAX - (align - 1)`) first, then the transmute inside a `const fn` called at
//! run time (core's `Alignment::new_unchecked` is a `const fn`), then the same copies and
//! compares. Returns 100 + n when `valid` said no, 1 + n for a wrong copy. Isolates: a `const
//! fn`'s body built for a run-time call (its own instance, which the lazy path makes on its
//! first call) versus an ordinary fn's, h4 below. 250 in (c1) and a trap or 1 + n in (c2) puts
//! the cause in the const-fn path.

use super::{MemoryStore, PASSED, alloc, fixture, run_whole, walk_into};

/// The lang items, the impls of `Copy` the loop needs, `transmute` as the pinned core declares
/// it, and `AlignmentEnum`'s 64 variants with core's `Alignment` and `Layout` shapes.
macro_rules! common {
    () => {
        "#![feature(no_core, lang_items, rustc_attrs, intrinsics)]
#![no_core]
#![no_main]
#![allow(internal_features, dead_code)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for usize {}
impl Copy for i32 {}
impl Copy for bool {}

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

#[repr(transparent)]
struct W(A64);

struct L {
    size: usize,
    align: W,
}

#[inline(never)]
fn copy(a: A64) -> A64 {
    a
}

#[inline(never)]
fn pass(l: L) -> L {
    l
}
"
    };
}

/// (c1): the transmute in an `#[inline(never)] unsafe fn` at run time.
const C1: &str = concat!(
    common!(),
    "
#[inline(never)]
unsafe fn make(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute::<usize, W>(align) } }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: usize = 0;
    while n < 64 {
        let want: usize = 1usize << n;
        let l = pass(unsafe { make(16, want) });
        if l.size != 16 {
            return (1 + n) as i32;
        }
        let a = copy(l.align.0);
        if a as usize != want {
            return (1 + n) as i32;
        }
        n += 1;
    }
    250
}
"
);

/// (c2): `valid` first, then the transmute in a `const fn` called at run time. Over n in 0..63:
/// an alignment of 2^63 leaves no room for a size of 16 (`valid` refuses it, as core's
/// `Layout::from_size_align` does; the first run returned 163 for exactly that, 2026-11-17).
/// `main` is `extern "C-unwind"` in both programs, as this file's other rows are: an
/// `extern "C"` one needs the `panic_cannot_unwind` lang item the fixture does not declare.
const C2: &str = concat!(
    common!(),
    "
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

const fn make_const(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute::<usize, W>(align) } }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: usize = 0;
    while n < 63 {
        let want: usize = 1usize << n;
        if !valid(16, want) {
            return (100 + n) as i32;
        }
        let l = pass(make_const(16, want));
        if l.size != 16 {
            return (1 + n) as i32;
        }
        let a = copy(l.align.0);
        if a as usize != want {
            return (1 + n) as i32;
        }
        n += 1;
    }
    250
}
"
);

/// `text` walked over names records and run whole, both before either is asserted (as
/// `row`). Prints "match layouts <name>: walk <counts or refusal>; status <n or refusal>".
fn small(name: &str, text: &str) {
    let f = fixture(text);
    let mut store = MemoryStore::new_in(alloc());
    let walked = walk_into(&f, &mut store);
    let ran = run_whole(text);
    match (&walked, &ran) {
        (Ok(counts), Ok(status)) => println!("match layouts {name}: walk {counts}; status {status}"),
        (Ok(counts), Err(e)) => println!("match layouts {name}: walk {counts}; status REFUSED: {e}"),
        (Err(e), Ok(status)) => println!("match layouts {name}: walk REFUSED: {e}; status {status}"),
        (Err(e), Err(r)) => println!("match layouts {name}: walk REFUSED: {e}; status REFUSED: {r}"),
    }
    if let Err(e) = walked {
        panic!("{name}: the lazy path: {e}");
    }
    match ran {
        Ok(status) => assert_eq!(status, PASSED, "{name}: 250 is every n passed; 1 + n a wrong copy of 1 << n; 100 + n `valid` refused it\n{text}"),
        Err(e) => panic!("{name}: the whole run refused: {e}"),
    }
}

/// (c1), expected 250.
#[test]
fn c1_all_sixty_four_through_a_small_main() {
    small("(c1) all 64, run-time transmute, small main", C1);
}

/// (c2), expected 250.
#[test]
fn c2_all_sixty_four_through_a_const_fn_after_a_check() {
    small("(c2) all 64, const fn transmute after valid", C2);
}

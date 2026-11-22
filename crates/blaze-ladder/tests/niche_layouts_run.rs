//! NICHE LAYOUTS as rustc makes them, run: each program is `#![no_core]`, built by the new front
//! end and run whole (`run_with`), and `main` returns 250 when every check passed, else the
//! number of the first check that failed (each named in its program).
//!
//! A niche's VALUE is not blaze's to choose: std's compiled code and a user's `transmute` read
//! it. The values here are rustc's (the pinned toolchain's crates/rustc_abi lib.rs
//! `Niche::reserve`: one value goes on the side nearer zero as a signed number, below the start
//! on a tie; more go past the end), the same values blaze-build's layout/niche_table.rs derives
//! row by row:
//!
//! - `Option<NonZero<u8>>`'s `None` is 0; `Option<char>`'s 0xFFFF_FFFF, `Option<Option<char>>`'s
//!   0xFFFF_FFFE (not 0x110000, 0x110001);
//! - `Option<Cap>` (core's `usize is 0..=isize::MAX`) is usize::MAX (not 2^63);
//! - `Option<Ordering>` is -2 (0xfe), `Option<Option<Ordering>>` 2;
//! - `Option<bool>` is 2, `Option<Option<bool>>` 0xff; `Option<Three>` 0xff;
//! - `Option<Gap>` (discriminants 0, 200, 100: the tag's range is the wrap-around `100..=0`) 1.
//!
//! The rows: (1) each `None` and a `Some` transmuted TO the scalar, (2) the scalar transmuted IN
//! and matched, (3) Some/None round trips by value and by reference and a nested option match,
//! (4) the fixture real_shape.rs had to leave out (`#[rustc_layout_scalar_valid_range_start(1)]
//! struct Inner(usize)` under `NZ(Inner)`, refused by both front-end halves before: heads.rs
//! read the attribute as a refusal flag), with `_end` beside it, (5) core's `NonZeroCharInner`,
//! a pattern type over `char`.
//!
//! The run's heap is one `blaze_mem::Root` made at the file's top and leaked for `run_with`'s
//! `SharedAlloc<'static>` (`Root::leak`, counted by the heap ratchet).

use std::sync::OnceLock;

use blaze_ladder::run::OverflowChecks;
use blaze_mem::{Root, RootOpts};
use blaze_own::SharedAlloc;

const PASSED: u8 = 250;

/// The lang items, `Option`, `transmute` as the pinned core declares it, core's
/// `pattern_type!`, and the shapes every program uses.
const HEAD: &str = "#![feature(no_core, lang_items, rustc_attrs, intrinsics, decl_macro, pattern_types, pattern_type_macro)]
#![no_core]
#![no_main]
#![allow(internal_features, dead_code, unreachable_patterns, unused_unsafe)]

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
impl Copy for usize {}
impl Copy for char {}
impl Copy for bool {}
impl<T: PointeeSized> Copy for &T {}

enum Option<T> {
    None,
    Some(T),
}

#[rustc_nounwind]
#[rustc_intrinsic]
pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;

#[rustc_builtin_macro(pattern_type)]
pub macro pattern_type($($arg:tt)*) {
    /* compiler built-in */
}

/// core's `NonZero<u8>` (num/niche_types.rs `NonZeroU8Inner`).
#[repr(transparent)]
struct Nz8(pattern_type!(u8 is 1..=255));

/// core's `Cap` (raw_vec.rs, `UsizeNoHighBit`: `usize is 0..=isize::MAX`).
#[repr(transparent)]
struct Cap(pattern_type!(usize is 0..=9223372036854775807));

#[repr(i8)]
enum Ordering {
    Less = -1,
    Equal = 0,
    Greater = 1,
}

enum Three {
    A,
    B,
    C,
}

enum Gap {
    A = 0,
    B = 200,
    C = 100,
}

#[inline(never)]
fn opt_nz8(k: u8) -> Option<Nz8> {
    match k {
        0 => Option::None,
        _ => Option::Some(unsafe { transmute::<u8, Nz8>(k) }),
    }
}

#[inline(never)]
fn opt_char(k: u8) -> Option<char> {
    match k {
        0 => Option::None,
        _ => Option::Some('a'),
    }
}

#[inline(never)]
fn opt_opt_char(k: u8) -> Option<Option<char>> {
    match k {
        0 => Option::None,
        1 => Option::Some(Option::None),
        _ => Option::Some(Option::Some('a')),
    }
}

#[inline(never)]
fn opt_cap(k: u8) -> Option<Cap> {
    match k {
        0 => Option::None,
        _ => Option::Some(unsafe { transmute::<usize, Cap>(5) }),
    }
}

#[inline(never)]
fn opt_ord(k: u8) -> Option<Ordering> {
    match k {
        0 => Option::None,
        1 => Option::Some(Ordering::Less),
        2 => Option::Some(Ordering::Equal),
        _ => Option::Some(Ordering::Greater),
    }
}

#[inline(never)]
fn opt_opt_ord(k: u8) -> Option<Option<Ordering>> {
    match k {
        0 => Option::None,
        1 => Option::Some(Option::None),
        2 => Option::Some(Option::Some(Ordering::Less)),
        3 => Option::Some(Option::Some(Ordering::Equal)),
        _ => Option::Some(Option::Some(Ordering::Greater)),
    }
}

#[inline(never)]
fn opt_bool(k: u8) -> Option<bool> {
    match k {
        0 => Option::None,
        _ => Option::Some(true),
    }
}

#[inline(never)]
fn opt_opt_bool(k: u8) -> Option<Option<bool>> {
    match k {
        0 => Option::None,
        1 => Option::Some(Option::None),
        _ => Option::Some(Option::Some(false)),
    }
}

#[inline(never)]
fn opt_three(k: u8) -> Option<Three> {
    match k {
        0 => Option::None,
        _ => Option::Some(Three::C),
    }
}

#[inline(never)]
fn opt_gap(k: u8) -> Option<Gap> {
    match k {
        0 => Option::None,
        1 => Option::Some(Gap::A),
        2 => Option::Some(Gap::B),
        _ => Option::Some(Gap::C),
    }
}
";

/// The run's heap (the module doc): one root for every test of this file.
fn shared() -> SharedAlloc<'static> {
    static ROOT: OnceLock<&'static Root> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = Root::new(RootOpts::TEST_ARENA).unwrap_or_else(|e| panic!("the test's root: {e}"));
        Root::leak(root).unwrap_or_else(|e| panic!("the test's root, kept for the process: {e}"))
    })
    .shared()
}

/// `HEAD` and `rest` built by the new front end and run whole: its status, or the refusal's text.
fn run_whole(rest: &str) -> (String, Result<u8, String>) {
    let text = format!("{HEAD}{rest}");
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text: &text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
    let ran = blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), shared()).map_err(|e| e.to_string());
    (text, ran)
}

/// Runs `rest`; prints "niche layouts <name>: status <n or refusal>" and asserts 250.
fn row(name: &str, rest: &str) {
    let (text, ran) = run_whole(rest);
    match &ran {
        Ok(status) => println!("niche layouts {name}: status {status}"),
        Err(e) => println!("niche layouts {name}: status REFUSED: {e}"),
    }
    let status = ran.unwrap_or_else(|e| panic!("{name}: the whole run refused: {e}"));
    assert_eq!(status, PASSED, "{name}: 250 is every check passed; else the number of the first failed check\n{text}");
}

/// (1) Each `None`, and a `Some`, transmuted TO the scalar: rustc's niche values (the module
/// doc). Checks 1..=22 in order.
#[test]
fn none_and_some_transmuted_to_the_scalar() {
    row(
        "(1) transmute to the scalar",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    match unsafe { transmute::<Option<Nz8>, u8>(opt_nz8(0)) } { 0 => {} _ => return 1 }
    match unsafe { transmute::<Option<Nz8>, u8>(opt_nz8(9)) } { 9 => {} _ => return 2 }
    match unsafe { transmute::<Option<char>, u32>(opt_char(0)) } { 4294967295 => {} _ => return 3 }
    match unsafe { transmute::<Option<char>, u32>(opt_char(1)) } { 97 => {} _ => return 4 }
    match unsafe { transmute::<Option<Option<char>>, u32>(opt_opt_char(0)) } { 4294967294 => {} _ => return 5 }
    match unsafe { transmute::<Option<Option<char>>, u32>(opt_opt_char(1)) } { 4294967295 => {} _ => return 6 }
    match unsafe { transmute::<Option<Option<char>>, u32>(opt_opt_char(2)) } { 97 => {} _ => return 7 }
    match unsafe { transmute::<Option<Cap>, usize>(opt_cap(0)) } { 18446744073709551615 => {} _ => return 8 }
    match unsafe { transmute::<Option<Cap>, usize>(opt_cap(1)) } { 5 => {} _ => return 9 }
    match unsafe { transmute::<Option<Ordering>, u8>(opt_ord(0)) } { 254 => {} _ => return 10 }
    match unsafe { transmute::<Option<Ordering>, u8>(opt_ord(1)) } { 255 => {} _ => return 11 }
    match unsafe { transmute::<Option<Option<Ordering>>, u8>(opt_opt_ord(0)) } { 2 => {} _ => return 12 }
    match unsafe { transmute::<Option<Option<Ordering>>, u8>(opt_opt_ord(1)) } { 254 => {} _ => return 13 }
    match unsafe { transmute::<Option<Option<Ordering>>, u8>(opt_opt_ord(4)) } { 1 => {} _ => return 14 }
    match unsafe { transmute::<Option<bool>, u8>(opt_bool(0)) } { 2 => {} _ => return 15 }
    match unsafe { transmute::<Option<bool>, u8>(opt_bool(1)) } { 1 => {} _ => return 16 }
    match unsafe { transmute::<Option<Option<bool>>, u8>(opt_opt_bool(0)) } { 255 => {} _ => return 17 }
    match unsafe { transmute::<Option<Option<bool>>, u8>(opt_opt_bool(1)) } { 2 => {} _ => return 18 }
    match unsafe { transmute::<Option<Three>, u8>(opt_three(0)) } { 255 => {} _ => return 19 }
    match unsafe { transmute::<Option<Gap>, u8>(opt_gap(0)) } { 1 => {} _ => return 20 }
    match unsafe { transmute::<Option<Gap>, u8>(opt_gap(2)) } { 200 => {} _ => return 21 }
    match unsafe { transmute::<Option<Gap>, u8>(opt_gap(3)) } { 100 => {} _ => return 22 }
    250
}
",
    );
}

/// (2) rustc's niche values transmuted IN and matched: each is the variant rustc's code means
/// by it. Checks 1..=14.
#[test]
fn the_scalar_transmuted_in_is_rustcs_variant() {
    row(
        "(2) transmute in",
        "
#[inline(never)]
fn which_nz8(x: Option<Nz8>) -> u8 {
    match x { Option::None => 1, Option::Some(_) => 2 }
}

#[inline(never)]
fn which_char(x: Option<char>) -> u8 {
    match x { Option::None => 1, Option::Some('b') => 2, Option::Some(_) => 3 }
}

#[inline(never)]
fn which_opt_char(x: Option<Option<char>>) -> u8 {
    match x { Option::None => 1, Option::Some(Option::None) => 2, Option::Some(Option::Some(_)) => 3 }
}

#[inline(never)]
fn which_cap(x: Option<Cap>) -> u8 {
    match x { Option::None => 1, Option::Some(_) => 2 }
}

#[inline(never)]
fn which_ord(x: Option<Option<Ordering>>) -> u8 {
    match x {
        Option::None => 1,
        Option::Some(Option::None) => 2,
        Option::Some(Option::Some(Ordering::Less)) => 3,
        Option::Some(Option::Some(Ordering::Equal)) => 4,
        Option::Some(Option::Some(Ordering::Greater)) => 5,
    }
}

#[inline(never)]
fn which_gap(x: Option<Gap>) -> u8 {
    match x { Option::None => 1, Option::Some(Gap::A) => 2, Option::Some(Gap::B) => 3, Option::Some(Gap::C) => 4 }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    match which_nz8(unsafe { transmute::<u8, Option<Nz8>>(0) }) { 1 => {} _ => return 1 }
    match which_nz8(unsafe { transmute::<u8, Option<Nz8>>(200) }) { 2 => {} _ => return 2 }
    match which_char(unsafe { transmute::<u32, Option<char>>(4294967295) }) { 1 => {} _ => return 3 }
    match which_char(unsafe { transmute::<u32, Option<char>>(98) }) { 2 => {} _ => return 4 }
    match which_opt_char(unsafe { transmute::<u32, Option<Option<char>>>(4294967294) }) { 1 => {} _ => return 5 }
    match which_opt_char(unsafe { transmute::<u32, Option<Option<char>>>(4294967295) }) { 2 => {} _ => return 6 }
    match which_opt_char(unsafe { transmute::<u32, Option<Option<char>>>(0) }) { 3 => {} _ => return 7 }
    match which_cap(unsafe { transmute::<usize, Option<Cap>>(18446744073709551615) }) { 1 => {} _ => return 8 }
    match which_cap(unsafe { transmute::<usize, Option<Cap>>(9223372036854775807) }) { 2 => {} _ => return 9 }
    match which_ord(unsafe { transmute::<u8, Option<Option<Ordering>>>(2) }) { 1 => {} _ => return 10 }
    match which_ord(unsafe { transmute::<u8, Option<Option<Ordering>>>(254) }) { 2 => {} _ => return 11 }
    match which_ord(unsafe { transmute::<u8, Option<Option<Ordering>>>(255) }) { 3 => {} _ => return 12 }
    match which_gap(unsafe { transmute::<u8, Option<Gap>>(1) }) { 1 => {} _ => return 13 }
    match which_gap(unsafe { transmute::<u8, Option<Gap>>(100) }) { 4 => {} _ => return 14 }
    250
}
",
    );
}

/// (3) Some/None ROUND TRIPS through `#[inline(never)]` fns by value and by reference, and a
/// NESTED option match over every variant of `Option<Option<Ordering>>` (the tag read through
/// two niche levels, by value and through `&`). Checks 1..=17.
#[test]
fn round_trips_and_a_nested_option_match() {
    row(
        "(3) round trips and nested match",
        "
#[inline(never)]
fn pass_ord(x: Option<Option<Ordering>>) -> Option<Option<Ordering>> {
    x
}

#[inline(never)]
fn pass_char(x: Option<Option<char>>) -> Option<Option<char>> {
    x
}

#[inline(never)]
fn pass_gap(x: Option<Gap>) -> Option<Gap> {
    x
}

#[inline(never)]
fn nested(x: Option<Option<Ordering>>) -> u8 {
    match x {
        Option::None => 1,
        Option::Some(Option::None) => 2,
        Option::Some(Option::Some(Ordering::Less)) => 3,
        Option::Some(Option::Some(Ordering::Equal)) => 4,
        Option::Some(Option::Some(Ordering::Greater)) => 5,
    }
}

#[inline(never)]
fn nested_ref(x: &Option<Option<Ordering>>) -> u8 {
    match x {
        Option::None => 1,
        Option::Some(Option::None) => 2,
        Option::Some(Option::Some(Ordering::Less)) => 3,
        Option::Some(Option::Some(Ordering::Equal)) => 4,
        Option::Some(Option::Some(Ordering::Greater)) => 5,
    }
}

#[inline(never)]
fn chars(x: &Option<Option<char>>) -> u8 {
    match x {
        Option::None => 1,
        Option::Some(Option::None) => 2,
        Option::Some(Option::Some('a')) => 3,
        Option::Some(Option::Some(_)) => 4,
    }
}

#[inline(never)]
fn gaps(x: Option<Gap>) -> u8 {
    match x { Option::None => 1, Option::Some(Gap::A) => 2, Option::Some(Gap::B) => 3, Option::Some(Gap::C) => 4 }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    match nested(pass_ord(opt_opt_ord(0))) { 1 => {} _ => return 1 }
    match nested(pass_ord(opt_opt_ord(1))) { 2 => {} _ => return 2 }
    match nested(pass_ord(opt_opt_ord(2))) { 3 => {} _ => return 3 }
    match nested(pass_ord(opt_opt_ord(3))) { 4 => {} _ => return 4 }
    match nested(pass_ord(opt_opt_ord(4))) { 5 => {} _ => return 5 }
    match nested_ref(&pass_ord(opt_opt_ord(0))) { 1 => {} _ => return 6 }
    match nested_ref(&pass_ord(opt_opt_ord(1))) { 2 => {} _ => return 7 }
    match nested_ref(&pass_ord(opt_opt_ord(2))) { 3 => {} _ => return 8 }
    match nested_ref(&pass_ord(opt_opt_ord(4))) { 5 => {} _ => return 9 }
    match chars(&pass_char(opt_opt_char(0))) { 1 => {} _ => return 10 }
    match chars(&pass_char(opt_opt_char(1))) { 2 => {} _ => return 11 }
    match chars(&pass_char(opt_opt_char(2))) { 3 => {} _ => return 12 }
    match gaps(pass_gap(opt_gap(0))) { 1 => {} _ => return 13 }
    match gaps(pass_gap(opt_gap(1))) { 2 => {} _ => return 14 }
    match gaps(pass_gap(opt_gap(2))) { 3 => {} _ => return 15 }
    match gaps(pass_gap(opt_gap(3))) { 4 => {} _ => return 16 }
    match opt_nz8(0) { Option::None => {} Option::Some(_) => return 17 }
    250
}
",
    );
}

/// (4) THE FIXTURE real_shape.rs left out: `#[rustc_layout_scalar_valid_range_start(1)]` on
/// `Inner(usize)` under the transparent `NZ(Inner)` (core's `NonZero<usize>` before pattern
/// types), and `_end(254)` on `Below(u8)`: `Option<NZ>`'s `None` is 0 (the tie below), and
/// `Option<Below>`'s 0xff (0..=254: next up and next down both -1, the tie below). Checks
/// 1..=8.
#[test]
fn valid_range_attributes_are_one_fact_with_pattern_types() {
    row(
        "(4) rustc_layout_scalar_valid_range_*",
        "
#[rustc_layout_scalar_valid_range_start(1)]
#[repr(transparent)]
struct Inner(usize);

#[repr(transparent)]
struct NZ(Inner);

#[rustc_layout_scalar_valid_range_end(254)]
#[repr(transparent)]
struct Below(u8);

#[inline(never)]
fn make(k: usize) -> Option<NZ> {
    match k {
        0 => Option::None,
        _ => Option::Some(unsafe { transmute::<usize, NZ>(k) }),
    }
}

#[inline(never)]
fn get(x: Option<NZ>) -> usize {
    match x {
        Option::Some(n) => unsafe { transmute::<NZ, usize>(n) },
        Option::None => 77,
    }
}

#[inline(never)]
fn below(k: u8) -> Option<Below> {
    match k {
        0 => Option::None,
        _ => Option::Some(unsafe { transmute::<u8, Below>(k) }),
    }
}

#[inline(never)]
fn which_below(x: Option<Below>) -> u8 {
    match x { Option::None => 1, Option::Some(_) => 2 }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    match unsafe { transmute::<Option<NZ>, usize>(make(0)) } { 0 => {} _ => return 1 }
    match unsafe { transmute::<Option<NZ>, usize>(make(40)) } { 40 => {} _ => return 2 }
    match get(make(0)) { 77 => {} _ => return 3 }
    match get(make(40)) { 40 => {} _ => return 4 }
    match get(unsafe { transmute::<usize, Option<NZ>>(0) }) { 77 => {} _ => return 5 }
    match unsafe { transmute::<Option<Below>, u8>(below(0)) } { 255 => {} _ => return 6 }
    match which_below(unsafe { transmute::<u8, Option<Below>>(255) }) { 1 => {} _ => return 7 }
    match which_below(unsafe { transmute::<u8, Option<Below>>(0) }) { 2 => {} _ => return 8 }
    250
}
",
    );
}

/// (5) core's `NonZeroCharInner`, `char is '\u{1}'..='\u{10FFFF}'`: the pattern REPLACES the
/// base's range (char's own `0..=0x10FFFF`), so `None` is 0 (next down 0). Checks 1..=3.
#[test]
fn a_pattern_type_over_char() {
    row(
        "(5) Option<char is 1..=0x10FFFF>",
        "
#[repr(transparent)]
struct NzChar(pattern_type!(char is '\\u{1}'..='\\u{10FFFF}'));

#[inline(never)]
fn make(k: u8) -> Option<NzChar> {
    match k {
        0 => Option::None,
        _ => Option::Some(unsafe { transmute::<char, NzChar>('z') }),
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    match unsafe { transmute::<Option<NzChar>, u32>(make(0)) } { 0 => {} _ => return 1 }
    match unsafe { transmute::<Option<NzChar>, u32>(make(1)) } { 122 => {} _ => return 2 }
    match unsafe { transmute::<u32, Option<NzChar>>(0) } { Option::None => {} Option::Some(_) => return 3 }
    250
}
",
    );
}

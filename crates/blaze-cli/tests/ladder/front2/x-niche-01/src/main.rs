// Own program x-niche-01 (front2): a pattern type whose range end is a CONSTANT, as core writes
// `UsizeNoHighBit` (library/core/src/num/niche_types.rs:113 and :129):
//     const HALF_USIZE: usize = usize::MAX >> 1;
//     pub struct UsizeNoHighBit(pattern_type!(usize is 0..=HALF_USIZE));
// (the struct through core's `define_valid_range_type!`, its field `pattern_type!($int is $pat)`).
// The Reference has no pattern types yet; their rule is the pinned library's own text,
// library/core/src/pat.rs: "Creates a pattern type. `type Positive = std::pat::pattern_type!(i32
// is 1..);`", a type of the base's layout whose values are those the pattern matches; the
// values outside are a niche, so `Option` of it takes no tag (core's `Option<NonZero<..>>` is
// the same size as the integer, num/nonzero.rs's doc: "`Option<NonZero<u32>>` is the same size
// as `u32`").
// What it takes of the seam: the head's range end `HALF` read as a constant (pattern_ty.rs's
// `array_len`, `Why::NeedConst`), evaluated by the ADT-row loop (program.rs, `eval_len`) and the
// head made again; the builder's layout reads the narrowed range (layout.rs `PNode::Pat`).
// `Lit` is the control: the same range with a literal end (no constant to evaluate).
// Status 42 only when `Half` is a `usize` and `Option<Half>` is no larger.
// `no_core`: `pattern_type!` is declared as core declares it, a builtin by name.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, pattern_types, pattern_type_macro)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}

#[rustc_builtin_macro(pattern_type)]
macro_rules! pattern_type {
    ($($arg:tt)*) => {
        /* compiler built-in */
    };
}

#[rustc_intrinsic]
const fn size_of<T>() -> usize;

/// `usize::MAX >> 1` on a 64-bit target, written as core's `HALF_USIZE` is: a shift.
const HALF: usize = !0 >> 1;

struct Half(pattern_type!(usize is 0..=HALF));

struct Lit(pattern_type!(usize is 0..=9223372036854775807));

enum Option<T> {
    None,
    Some(T),
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let word = size_of::<usize>();
    let control = size_of::<Lit>() == word && size_of::<Option<Lit>>() == word;
    let niche = size_of::<Half>() == word && size_of::<Option<Half>>() == word;
    if control && niche { 42 } else { 1 }
}

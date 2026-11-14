//! The draws blaze's tests need beyond the tape's own: booleans, options, arrays, bytes,
//! characters, text, floats, 128-bit integers, weighted picks and maps, and [`Any`], a draw by
//! type (what `#[blaze::contract_test]` draws a function's arguments with).
//!
//! Everything here is built from [`Tape`]'s public draws, so it replays and shrinks with no code
//! of its own. Each draw arranges its kinds so the calm choice (0) is the first kind, and the
//! all-zero tape gives the simplest value everywhere: `false`, `None`, a range's start, 0.0, a
//! space, an alphabet's first character, the least size, the first alternative with a weight.
//! Each element of a list, array or map is drawn in a span, so the shrinker deletes it whole.

#![blaze::sans_io]

use core::ops::RangeInclusive;
use std::alloc::Global;
use std::collections::BTreeMap;

use crate::tape::Tape;

#[cfg(test)]
mod tests;

/// The surrogate code points, which are no `char`: a draw of a code point is mapped past them.
const SURROGATES: RangeInclusive<u64> = 0xD800..=0xDFFF;
/// How many surrogate code points there are.
const SURROGATE_COUNT: u64 = 0x800;
/// The printable ASCII characters, from the space.
const PRINTABLE_FROM: u64 = 0x20;
const PRINTABLE_COUNT: u64 = 0x7F - 0x20;
/// The basic plane's scalar values.
const BASIC_COUNT: u64 = 0x1_0000 - SURROGATE_COUNT;
/// Every scalar value.
const SCALAR_COUNT: u64 = 0x11_0000 - SURROGATE_COUNT;

/// How many draws in a row may give a key a map already holds before `btree_map` refuses.
const SAME_KEYS: u32 = 64;

/// The kinds `option` picks among: kind 0 (the calm one) is `None` and the other three are
/// `Some`, so a seeded `option` is `Some` three times in four, enough to reach the inner draw
/// often while `None` still turns up in every few cases.
const OPTION_KINDS: u64 = 4;

/// The kinds `f64_in` and `f32_in` pick among: the start (the calm one), the end, and two kinds
/// uniform between them, so each end turns up once in four, where an off-by-one at a bound hides.
const IN_RANGE_KINDS: u64 = 4;

/// How many values a byte has: `bytes` draws each byte uniform below this.
const BYTE_VALUES: u64 = 1 << u8::BITS;

/// The eighths `char` splits its kinds into: five for printable ASCII (`0..=4`), two for the rest
/// of the basic plane (`5 | 6`) and one for any scalar value, so most characters are readable
/// and a multi-byte or astral one still turns up in every few draws.
const CHAR_KINDS: u64 = 8;

/// The kinds `f64` and `f32` pick among, the calm one first: a small whole number, a small
/// integer over a power of two, uniform in `[0, 1)` of either sign, any bit pattern, and one of
/// the special values.
const FLOAT_KINDS: u64 = 5;

/// The base of `f64`'s exponent: a whole number times a power of it is exact while it fits the
/// significand, so the second float kind gives exact fractions.
const BINARY_BASE_F64: f64 = 2.0;

/// [`BINARY_BASE_F64`] as an `f32`.
const BINARY_BASE_F32: f32 = 2.0;

/// The most elements `Any for Vec<T>` and characters `Any for String` draw: the most a contract
/// test's argument needs to show a case, while each case stays small to run and to read.
const ANY_LONGEST: usize = 16;

/// The whole numbers a float's first kind draws, as zigzag offsets: magnitudes up to 2^53 (2^24
/// for `f32`), where every whole number is exact.
const WHOLE_F64: u64 = 1 << 54;
const WHOLE_F32: u64 = 1 << 25;

/// The `f64` values tests break on, 0.0 first (the calm one). The least subnormal is here so a
/// subnormal is as likely as any other special value; a bit pattern gives one only once in 2,048.
const SPECIAL_F64: [f64; 12] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    f64::MIN_POSITIVE,
    f64::EPSILON,
    f64::MAX,
    f64::MIN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
    f64::from_bits(1),
];

/// [`SPECIAL_F64`] as `f32`s.
const SPECIAL_F32: [f32; 12] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    f32::MIN_POSITIVE,
    f32::EPSILON,
    f32::MAX,
    f32::MIN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NAN,
    f32::from_bits(1),
];

/// 0, -1, 1, -2, 2, ..: small canonical values are small magnitudes of either sign.
#[expect(
    blaze::truncating_cast,
    reason = "`u >> 1` is at most `i64::MAX` and `u & 1` is 0 or 1, so neither cast changes a value"
)]
fn zigzag_64(u: u64) -> i64 {
    (u >> 1).cast_signed() ^ -(u & 1).cast_signed()
}

/// [`zigzag_64`] over 128 bits.
#[expect(
    blaze::truncating_cast,
    reason = "`u >> 1` is at most `i128::MAX` and `u & 1` is 0 or 1, so neither cast changes a value"
)]
fn zigzag_128(u: u128) -> i128 {
    (u >> 1).cast_signed() ^ -(u & 1).cast_signed()
}

/// A zigzag whole number of magnitude at most 2^53, as the `f64` of that value: the first float
/// kind, and the numerator of the second.
#[expect(
    blaze::as_cast,
    reason = "the cast is the operation: an integer of magnitude at most 2^53 made into the `f64` of the same value, which is exact"
)]
fn whole_f64(tape: &Tape, label: &'static str) -> f64 {
    zigzag_64(tape.int_in::<u64>(0..=WHOLE_F64, label)) as f64
}

/// [`whole_f64`] for `f32`: magnitude at most 2^24.
#[expect(
    blaze::as_cast,
    reason = "the cast is the operation: an integer of magnitude at most 2^24 made into the `f32` of the same value, which is exact"
)]
fn whole_f32(tape: &Tape, label: &'static str) -> f32 {
    zigzag_64(tape.int_in::<u64>(0..=WHOLE_F32, label)) as f32
}

/// How many times the second float kind halves its whole number: from 1 to `fraction_bits`
/// (the float's fraction width), so the result keeps every bit of a small numerator.
#[expect(
    blaze::as_cast,
    reason = "a draw below a fraction width (52 or 23) made into the `i32` exponent `powi` takes; every such value fits"
)]
fn halvings(tape: &Tape, fraction_bits: u32, label: &'static str) -> i32 {
    1 + tape.below(u64::from(fraction_bits), label) as i32
}

/// Where a float lies for `f64_finite` and `f32_finite`: finite, or past one end of the finite
/// values (a NaN counts as past the top).
enum Reach {
    Finite,
    AboveMax,
    BelowMin,
}

impl Reach {
    /// Where `x` lies; an `f32` is asked as its `f64`, which keeps its finiteness and sign.
    fn of(x: f64) -> Reach {
        if x.is_finite() {
            Reach::Finite
        } else if x.is_nan() || x > 0.0 {
            Reach::AboveMax
        } else {
            Reach::BelowMin
        }
    }
}

/// A count of non-surrogate code points from 0, as the code point it names.
fn past_surrogates(at: u64) -> u64 {
    match at >= *SURROGATES.start() {
        true => at + SURROGATE_COUNT,
        false => at,
    }
}

impl Tape {
    /// `coin` by its other name: calm is false.
    pub fn bool(&self, label: &'static str) -> bool {
        self.coin(label)
    }

    /// Calm is `None`; seeded, `Some` three times in four. `some`'s draws are one span.
    pub fn option<T>(&self, label: &'static str, some: impl FnOnce(&Tape) -> T) -> Option<T> {
        match self.below(OPTION_KINDS, label) {
            0 => None,
            _ => Some(self.span(label, some)),
        }
    }

    /// One of `items`; calm is the first. An empty `items` panics, naming the label.
    pub fn one_of<'a, T>(&self, items: &'a [T], label: &'static str) -> &'a T {
        match self.pick(items, label) {
            Some(item) => item,
            None => panic!("one of nothing (`{label}`)"),
        }
    }

    /// `N` elements, each one's draws a span.
    pub fn array<T, const N: usize>(
        &self,
        label: &'static str,
        mut element: impl FnMut(&Tape) -> T,
    ) -> [T; N] {
        core::array::from_fn(|_| self.span(label, &mut element))
    }

    /// `sizes` bytes, each uniform and its own span; calm is the least size of zeros.
    #[expect(
        blaze::as_cast,
        reason = "the cast is the operation: a draw below 256 taken as the byte of the same value, which every such draw is"
    )]
    pub fn bytes(&self, sizes: RangeInclusive<usize>, label: &'static str) -> Vec<u8> {
        self.vec(sizes, label, |tape| tape.below(BYTE_VALUES, label) as u8)
    }

    /// Any character: printable ASCII most often (five times in eight), then the rest of the
    /// basic plane (twice in eight), then any scalar value; calm is a space. Never a surrogate:
    /// the draw counts scalar values only.
    #[expect(
        blaze::dropped_error,
        reason = "a code that does not fit `u32` is no scalar value either, and the one `expect` that follows says both"
    )]
    pub fn char(&self, label: &'static str) -> char {
        let code = match self.below(CHAR_KINDS, label) {
            0..=4 => PRINTABLE_FROM + self.below(PRINTABLE_COUNT, label),
            5 | 6 => past_surrogates(self.below(BASIC_COUNT, label)),
            _ => past_surrogates(self.below(SCALAR_COUNT, label)),
        };
        u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .expect("a count of scalar values mapped past the surrogates is a scalar value")
    }

    /// `sizes` characters of `char`, each its own span; calm is the least size of spaces.
    pub fn text(&self, sizes: RangeInclusive<usize>, label: &'static str) -> String {
        let mut out = String::new();
        // The elements are pushed as they are drawn, so `vec` decides the length and the spans
        // and its own list holds only `()`, which allocates nothing.
        let _drawn: Vec<()> = self.vec(sizes, label, |tape| out.push(tape.char(label)));
        out
    }

    /// `sizes` characters, each one of `alphabet`'s and its own span; calm is its first. An
    /// empty alphabet panics, naming the label.
    pub fn text_from(
        &self,
        alphabet: &str,
        sizes: RangeInclusive<usize>,
        label: &'static str,
    ) -> String {
        let count = alphabet.chars().count();
        assert!(
            count > 0,
            "`{label}`: text_from with an empty alphabet: there is no character to draw"
        );
        let mut out = String::new();
        // As in `text`: `vec` decides the length and the spans; the characters go straight in.
        let _drawn: Vec<()> = self.vec(sizes, label, |tape| {
            let at = tape.index(count, label);
            out.push(alphabet.chars().nth(at).expect("`index` is below the alphabet's length"));
        });
        out
    }

    /// Any `f64`, NaN and the infinities among them; calm is 0.0. A kind first, the calm one
    /// first: a small whole number; a small integer over a power of two; uniform in `[0, 1)` of
    /// either sign; any bit pattern; one of the special values.
    pub fn f64(&self, label: &'static str) -> f64 {
        match self.below(FLOAT_KINDS, label) {
            0 => whole_f64(self, label),
            1 => {
                let whole = whole_f64(self, label);
                let power = halvings(self, f64::MANTISSA_DIGITS - 1, label);
                whole * BINARY_BASE_F64.powi(-power)
            }
            2 => {
                let negative = self.coin(label);
                let unit = self.unit_f64(label);
                match negative {
                    true => -unit,
                    false => unit,
                }
            }
            3 => f64::from_bits(self.next_u64(label)),
            _ => *self.one_of(&SPECIAL_F64, label),
        }
    }

    /// Any finite `f64`; calm is 0.0. `f64`'s draw with a NaN or infinite result replaced by
    /// `MAX` of the result's sign (a NaN counts as positive).
    pub fn f64_finite(&self, label: &'static str) -> f64 {
        let x = self.f64(label);
        match Reach::of(x) {
            Reach::Finite => x,
            Reach::AboveMax => f64::MAX,
            Reach::BelowMin => f64::MIN,
        }
    }

    /// A finite `f64` in `range`; calm is its start, either end once in four, else uniform
    /// between them. A range that is empty or has a NaN or infinite end panics, naming the label.
    pub fn f64_in(&self, range: RangeInclusive<f64>, label: &'static str) -> f64 {
        let (lo, hi) = range.into_inner();
        assert!(
            lo.is_finite() && hi.is_finite(),
            "`{label}`: f64_in over {lo}..={hi}: an end is NaN or infinite, and the draw is of finite values"
        );
        assert!(lo <= hi, "`{label}`: f64_in over {lo}..={hi}: the range is empty");
        match self.below(IN_RANGE_KINDS, label) {
            0 => lo,
            1 => hi,
            _ => {
                let unit = self.unit_f64(label);
                // Weighted ends, not `lo + unit * (hi - lo)`: `hi - lo` overflows to infinity
                // over a range wider than `MAX`; each term here stays finite.
                (lo * (1.0 - unit) + hi * unit).clamp(lo, hi)
            }
        }
    }

    /// Any `f32`, NaN and the infinities among them; calm is 0.0. `f64`'s kinds, drawn as `f32`
    /// (never an `f64` narrowed, which would miss `f32`'s own subnormals and bit patterns).
    pub fn f32(&self, label: &'static str) -> f32 {
        match self.below(FLOAT_KINDS, label) {
            0 => whole_f32(self, label),
            1 => {
                let whole = whole_f32(self, label);
                let power = halvings(self, f32::MANTISSA_DIGITS - 1, label);
                whole * BINARY_BASE_F32.powi(-power)
            }
            2 => {
                let negative = self.coin(label);
                let unit = unit_f32(self, label);
                match negative {
                    true => -unit,
                    false => unit,
                }
            }
            3 => f32::from_bits(self.next_u32(label)),
            _ => *self.one_of(&SPECIAL_F32, label),
        }
    }

    /// Any finite `f32`; calm is 0.0. As `f64_finite`.
    pub fn f32_finite(&self, label: &'static str) -> f32 {
        let x = self.f32(label);
        match Reach::of(f64::from(x)) {
            Reach::Finite => x,
            Reach::AboveMax => f32::MAX,
            Reach::BelowMin => f32::MIN,
        }
    }

    /// A finite `f32` in `range`; calm is its start. As `f64_in`.
    pub fn f32_in(&self, range: RangeInclusive<f32>, label: &'static str) -> f32 {
        let (lo, hi) = range.into_inner();
        assert!(
            lo.is_finite() && hi.is_finite(),
            "`{label}`: f32_in over {lo}..={hi}: an end is NaN or infinite, and the draw is of finite values"
        );
        assert!(lo <= hi, "`{label}`: f32_in over {lo}..={hi}: the range is empty");
        match self.below(IN_RANGE_KINDS, label) {
            0 => lo,
            1 => hi,
            _ => {
                let unit = unit_f32(self, label);
                // As in `f64_in`: each term stays finite where `hi - lo` would not.
                (lo * (1.0 - unit) + hi * unit).clamp(lo, hi)
            }
        }
    }

    /// Two `int::<u64>` halves, the high one first; calm is 0.
    pub fn u128(&self, label: &'static str) -> u128 {
        let high = self.int::<u64>(label);
        let low = self.int::<u64>(label);
        (u128::from(high) << u64::BITS) | u128::from(low)
    }

    /// `u128`, zigzag-decoded (0, -1, 1, -2, ..); calm is 0.
    pub fn i128(&self, label: &'static str) -> i128 {
        zigzag_128(self.u128(label))
    }

    /// Which of `weights.len()` alternatives, each as likely as its weight; calm is the first
    /// with a weight above 0, and an alternative of weight 0 is never the result. No weights, or
    /// all 0, panics naming the label.
    pub fn weighted(&self, weights: &[u32], label: &'static str) -> usize {
        assert!(
            !weights.is_empty(),
            "`{label}`: weighted with no weights: there is no alternative to pick"
        );
        let total: u64 = weights.iter().map(|&weight| u64::from(weight)).sum();
        assert!(
            total > 0,
            "`{label}`: weighted with every one of {} weights 0: no alternative can be picked",
            weights.len()
        );
        // Each alternative owns a stretch of `0..total` as long as its weight, in order, so 0
        // falls in the first one with a weight and an empty stretch is never hit.
        let mut left = self.below(total, label);
        for (at, &weight) in weights.iter().enumerate() {
            let weight = u64::from(weight);
            if left < weight {
                return at;
            }
            left -= weight;
        }
        unreachable!("`{label}`: `below({total})` gave a value at or past the sum of the weights")
    }

    /// A map of `sizes` entries, each entry's draws a span (a key that is already in the map is
    /// drawn again in the same span): keys are drawn until they are distinct, or 64 draws in a
    /// row gave a key already there, which panics naming the label.
    pub fn btree_map<K: Ord, V>(
        &self,
        sizes: RangeInclusive<usize>,
        label: &'static str,
        mut key: impl FnMut(&Tape) -> K,
        mut value: impl FnMut(&Tape) -> V,
    ) -> BTreeMap<K, V> {
        let (least, most) = (*sizes.start(), *sizes.end());
        let mut map = BTreeMap::new();
        // `vec` decides how many entries and opens each one's span; the entries go straight in,
        // so its own list holds only `()`, which allocates nothing.
        let _drawn: Vec<()> = self.vec(sizes, label, |tape| {
            for _ in 0..SAME_KEYS {
                let drawn = key(tape);
                if !map.contains_key(&drawn) {
                    let drawn_value = value(tape);
                    map.insert(drawn, drawn_value);
                    return;
                }
            }
            panic!(
                "`{label}`: btree_map drew a key already in the map {SAME_KEYS} times in a row, with {} of \
                 {least}..={most} entries: the key draw does not give enough distinct keys",
                map.len()
            );
        });
        map
    }

    /// A `T` by its own rule.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn any<T: Any>(&self, label: &'static str) -> T {
        T::any(self, label)
    }
}

/// Uniform in `[0, 1)` on `f32`'s 24-bit grid; calm is 0.0.
#[expect(
    blaze::as_cast,
    reason = "the cast is the operation: a draw below 2^24 and 2^24 itself made into the `f32`s of the same values, which are exact"
)]
fn unit_f32(tape: &Tape, label: &'static str) -> f32 {
    tape.below(1 << f32::MANTISSA_DIGITS, label) as f32
        * (1.0 / (1u32 << f32::MANTISSA_DIGITS) as f32)
}

/// A type with a draw of its own: what `#[blaze::contract_test]` draws a function's arguments
/// with. Calm is the type's simplest value (0, false, empty, `None`).
pub trait Any: Sized {
    fn any(tape: &Tape, label: &'static str) -> Self;
}

impl Any for bool {
    fn any(tape: &Tape, label: &'static str) -> bool {
        tape.bool(label)
    }
}

impl Any for char {
    fn any(tape: &Tape, label: &'static str) -> char {
        tape.char(label)
    }
}

/// The integers `Tape::int` draws.
macro_rules! any_int {
    ($($int:ty),+) => {
        $(
            impl Any for $int {
                fn any(tape: &Tape, label: &'static str) -> $int {
                    tape.int::<$int>(label)
                }
            }
        )+
    };
}

any_int!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl Any for u128 {
    fn any(tape: &Tape, label: &'static str) -> u128 {
        tape.u128(label)
    }
}

impl Any for i128 {
    fn any(tape: &Tape, label: &'static str) -> i128 {
        tape.i128(label)
    }
}

impl Any for f32 {
    fn any(tape: &Tape, label: &'static str) -> f32 {
        tape.f32(label)
    }
}

impl Any for f64 {
    fn any(tape: &Tape, label: &'static str) -> f64 {
        tape.f64(label)
    }
}

impl Any for () {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn any(_tape: &Tape, _label: &'static str) {}
}

/// Tuples: each element by its own rule, in order.
macro_rules! any_tuple {
    ($($element:ident),+) => {
        impl<$($element: Any),+> Any for ($($element,)+) {
            fn any(tape: &Tape, label: &'static str) -> Self {
                ($($element::any(tape, label),)+)
            }
        }
    };
}

any_tuple!(A);
any_tuple!(A, B);
any_tuple!(A, B, C);
any_tuple!(A, B, C, D);
any_tuple!(A, B, C, D, E);
any_tuple!(A, B, C, D, E, F);

impl<T: Any, const N: usize> Any for [T; N] {
    fn any(tape: &Tape, label: &'static str) -> [T; N] {
        tape.array(label, |tape| T::any(tape, label))
    }
}

impl<T: Any> Any for Option<T> {
    fn any(tape: &Tape, label: &'static str) -> Option<T> {
        tape.option(label, |tape| T::any(tape, label))
    }
}

/// Up to [`ANY_LONGEST`] elements.
impl<T: Any> Any for Vec<T> {
    fn any(tape: &Tape, label: &'static str) -> Vec<T> {
        tape.vec(0..=ANY_LONGEST, label, |tape| T::any(tape, label))
    }
}

/// Up to [`ANY_LONGEST`] characters of `Tape::char`.
impl Any for String {
    fn any(tape: &Tape, label: &'static str) -> String {
        tape.text(0..=ANY_LONGEST, label)
    }
}

impl<T: Any> Any for Box<T> {
    #[blaze::nonblocking]
    fn any(tape: &Tape, label: &'static str) -> Box<T> {
        Box::new_in(T::any(tape, label), Global)
    }
}

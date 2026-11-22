//! Every writer against `core::fmt` (boundary and random values), with a planted-bug control
//! for the two-digit table; floats against `{:?}` and `{:.N}` on random bits and the known
//! hard cases, and read back; random decimal text against `str::parse`; parse offsets; the
//! fixed buffer's bound; escapes; human units; the adapter.

use core::fmt;
use std::{
    fmt::{Binary, Display, LowerHex, UpperHex},
    format,
    string::{String as StdString, ToString},
    vec::Vec as StdVec,
};

use blaze_own::{Alloc, FixedString, Overflow, ParseIn, String, Vec, planted::Planted};
use blaze_sim::{Broken, Config, NoPlant, Tape, controls};

use crate::{
    Count, Fixed, Float, IntSyntax, Integer, Parse, ParseError, Put, Show, Shown, TextPlant, Unquoted, Writes,
    parse::{opt, sep_by, tag, take_while},
    parse_all, parse_int, parse_quoted_in,
};

/// The tests' allocator: blaze-own's test heap.
type Heap = Alloc<'static>;

fn heap() -> Heap {
    blaze_own::testing::heap().alloc()
}

/// What an oracle saw blaze-text get wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
enum TextBroken {
    /// A writer's text is not `core::fmt`'s (or `format!`'s) for the same value.
    Wrote { what: &'static str, value: StdString, got: StdString, want: StdString },
    /// A finite float's text did not read back as the same bits.
    ReadBack { value: StdString, written: StdString, read: StdString },
    /// Decimal text read as other float bits than core reads.
    FloatRead { text: StdString, got: Option<u64>, want: Option<u64> },
    /// `decimal_u64` took another length than the digit run.
    DecimalLength { at: usize },
    /// `decimal_u64` read another value than core.
    DecimalRead { at: usize, got: Option<u64>, want: Option<u64> },
    /// The vector `next_special` is not its scalar twin.
    NextSpecial { from: usize, at: usize, vector: usize, scalar: usize },
    /// The vector `hex16` is not its scalar twin.
    Hex16 { at: usize, vector: StdString, scalar: StdString },
    /// An integer read as another value than core reads.
    IntRead { text: StdString, got: Option<i128>, want: Option<i128> },
    /// The debug form is not `{:?}`'s.
    Debug { ours: StdString, core: StdString },
    /// `text_exact_in` kept more bytes than the text.
    NotExact { capacity: usize, len: usize },
}

impl fmt::Display for TextBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextBroken::Wrote { what, value, got, want } => {
                write!(f, "{what} of {value}: blaze-text wrote {got:?}, core::fmt {want:?}")
            }
            TextBroken::ReadBack { value, written, read } => {
                write!(f, "{value} wrote {written:?}, which read back as {read}")
            }
            TextBroken::FloatRead { text, got, want } => write!(f, "{text:?} read as bits {got:?}; core reads {want:?}"),
            TextBroken::DecimalLength { at } => write!(f, "decimal_u64 at {at}: wrong length"),
            TextBroken::DecimalRead { at, got, want } => write!(f, "decimal_u64 at {at}: read {got:?}, core {want:?}"),
            TextBroken::NextSpecial { from, at, vector, scalar } => {
                write!(f, "next_special from {from} at {at}: vector {vector}, scalar {scalar}")
            }
            TextBroken::Hex16 { at, vector, scalar } => write!(f, "hex16 at {at}: vector {vector}, scalar {scalar}"),
            TextBroken::IntRead { text, got, want } => write!(f, "{text:?}: blaze-text read {got:?}, core {want:?}"),
            TextBroken::Debug { ours, core } => write!(f, "blaze-text's debug form wrote {ours:?}, `{{:?}}` {core:?}"),
            TextBroken::NotExact { capacity, len } => write!(f, "text_exact_in kept {capacity} bytes for {len}"),
        }
    }
}

fn text(write: impl FnOnce(&mut String<Heap>)) -> StdString {
    let mut out = String::new_in(heap());
    write(&mut out);
    StdString::from(out.as_str())
}

fn same(what: &'static str, shown: &dyn Display, got: StdString, want: StdString) -> Result<(), Broken<TextBroken>> {
    match got == want {
        true => Ok(()),
        false => Err(Broken::Oracle(TextBroken::Wrote { what, value: format!("{shown}"), got, want })),
    }
}

// Integers.

fn one_int<I: Integer + Show + Display + LowerHex + UpperHex + Binary>(
    x: I,
    plant: impl Planted<TextPlant>,
) -> Result<(), Broken<TextBroken>> {
    let full = crate::int::nibbles::<I>();
    // First, the path every decimal writer takes, under the control's plant.
    let planted = StdString::from(crate::int::decimal_planted(x, plant).as_str());
    same("planted decimal", &x, planted, format!("{x}"))?;
    same("decimal", &x, text(|o| _ = o.int(x)), format!("{x}"))?;
    same("hex", &x, text(|o| _ = o.hex(x)), format!("{x:x}"))?;
    same("upper hex", &x, text(|o| _ = o.hex_upper(x)), format!("{x:X}"))?;
    same("full hex", &x, text(|o| _ = o.hex_full(x)), format!("{x:0full$x}"))?;
    same("fixed hex", &x, text(|o| _ = o.hex_fixed(x, 40)), format!("{x:040x}"))?;
    same("binary", &x, text(|o| _ = o.bin(x)), format!("{x:b}"))?;
    same("padded decimal", &x, text(|o| _ = o.int_padded(x, 30, ' ')), format!("{x:>30}"))?;
    same("zero-padded decimal", &x, text(|o| _ = o.int_zeros(x, 12)), format!("{x:012}"))?;
    same("right column", &x, text(|o| _ = o.right(&x, 25)), format!("{x:>25}"))?;
    same("left column", &x, text(|o| _ = o.left(&x, 25)), format!("{x:<25}"))?;
    same("Shown", &x, format!("{:>44}", Shown(x)), format!("{x:>44}"))?;
    Ok(())
}

/// `T` from the low bytes of `bits`.
macro_rules! from_bits {
    ($t:ident, $bits:expr) => {{
        let bytes = $bits.to_le_bytes();
        let mut low = [0u8; core::mem::size_of::<$t>()];
        low.copy_from_slice(&bytes[..core::mem::size_of::<$t>()]);
        $t::from_le_bytes(low)
    }};
}

macro_rules! ints {
    ($tape:ident, $plant:ident; $($t:ident)*) => {$(
        // The boundaries: MIN, MAX, and each power of ten and its neighbours.
        for x in [$t::MIN, $t::MAX, $t::MIN.wrapping_add(1), $t::MAX.wrapping_sub(1)] {
            one_int(x, $plant)?;
        }
        let ten = $t::try_from(10u8).unwrap_or($t::MAX);
        let mut power = Some(ten);
        while let Some(p) = power {
            for x in [p.wrapping_sub(1), p, p.wrapping_add(1)] {
                one_int(x, $plant)?;
                one_int(x.wrapping_neg(), $plant)?;
            }
            power = p.checked_mul(ten);
        }
        // Random values of every length.
        for _ in 0..8 {
            let bits = $tape.u128("bits") >> $tape.below(128, "shift");
            one_int(from_bits!($t, bits), $plant)?;
        }
    )*};
}

/// Every integer writer against `core::fmt`; the decimal path takes `plant` ([`NoPlant`] for
/// the clean run).
fn int_oracle<P: Planted<TextPlant>>(tape: &Tape, plant: P) -> Result<(), Broken<TextBroken>> {
    ints!(tape, plant; u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize);
    Ok(())
}

blaze_sim::plants! {
    /// The bug planted in the integer path, with the invariant of the integer oracle that must
    /// catch it.
    enum IntControl breaks TextBroken {
        /// The writers as they are.
        None,
        /// The two-digit table read one pair off: the planted decimal path is not `{}`.
        DigitPairOffByOne => TextBroken::Wrote { what: "planted decimal", .. },
    }
}

impl IntControl {
    /// The crate's plant this control plants.
    fn planted(self) -> TextPlant {
        match self {
            IntControl::None => TextPlant::None,
            IntControl::DigitPairOffByOne => TextPlant::DigitPairOffByOne,
        }
    }
}

#[test]
fn every_integer_writer_matches_core_fmt() {
    blaze_sim::check(Config::cases(200), |tape| int_oracle(tape, NoPlant));
}

#[test]
fn every_integer_plant_is_caught_by_its_invariant() {
    controls(Config::cases(50), |tape, plant: IntControl| int_oracle(tape, plant.planted()));
}

// Floats.

const HARD_F64: [f64; 16] = [
    5e-324,
    f64::MAX,
    f64::MIN_POSITIVE,
    2.225_073_858_507_201e-308,
    9_007_199_254_740_993.0,
    0.1,
    0.3,
    1e23,
    8.988_465_674_311_58e307,
    1e16,
    9_999_999_999_999_998.0,
    1e-4,
    9.999_999_999_999_999e-5,
    123_456.789,
    1.0,
    -0.0,
];

fn display_text<F: Float>(x: F) -> StdString {
    text(|o| _ = o.float(x))
}

fn debug_text<F: Float>(x: F) -> StdString {
    text(|o| _ = o.float_debug(x))
}

fn check_f64(x: f64) -> Result<(), Broken<TextBroken>> {
    same("shortest f64", &x, debug_text(x), format!("{x:?}"))?;
    same("display f64", &x, display_text(x), format!("{x}"))?;
    same("Show f64", &x, text(|o| _ = o.show(&x)), format!("{x}"))?;
    if x.is_finite() {
        for written in [display_text(x), debug_text(x)] {
            match parse_all::<f64>(&written) {
                Ok(back) if back.to_bits() == x.to_bits() => {}
                other => {
                    let (value, read) = (format!("{x:?}"), format!("{other:?}"));
                    return Err(Broken::Oracle(TextBroken::ReadBack { value, written, read }));
                }
            }
        }
    }
    let places = [0usize, 1, 2, 3, 6, 17, 20, 30];
    for p in places {
        same("fixed f64", &x, text(|o| _ = o.float_fixed(x, p)), format!("{x:.p$}"))?;
    }
    Ok(())
}

fn check_f32(x: f32) -> Result<(), Broken<TextBroken>> {
    same("shortest f32", &x, debug_text(x), format!("{x:?}"))?;
    same("display f32", &x, display_text(x), format!("{x}"))?;
    same("Show f32", &x, text(|o| _ = o.show(&x)), format!("{x}"))?;
    if x.is_finite() {
        for written in [display_text(x), debug_text(x)] {
            match parse_all::<f32>(&written) {
                Ok(back) if back.to_bits() == x.to_bits() => {}
                other => {
                    let (value, read) = (format!("{x:?}"), format!("{other:?}"));
                    return Err(Broken::Oracle(TextBroken::ReadBack { value, written, read }));
                }
            }
        }
    }
    for p in [0usize, 2, 9, 20] {
        same("fixed f32", &x, text(|o| _ = o.float_fixed(x, p)), format!("{x:.p$}"))?;
    }
    Ok(())
}

#[test]
fn hard_floats_match_core_fmt_and_read_back() {
    for x in HARD_F64 {
        if let Err(broke) = check_f64(x).and_then(|()| check_f64(-x)) {
            panic!("{broke:?}");
        }
    }
    for x in [f32::MAX, f32::MIN_POSITIVE, 1e-45, 0.1, 16_777_217.0, 1e16, 3.4e38, 7.038_531e-26] {
        if let Err(broke) = check_f32(x).and_then(|()| check_f32(-x)) {
            panic!("{broke:?}");
        }
    }
    let specials = [(f64::NAN, "NaN", "NaN"), (f64::INFINITY, "inf", "inf"), (f64::NEG_INFINITY, "-inf", "-inf"), (0.0, "0", "0.0")];
    for (x, display, debug) in specials {
        assert_eq!(display_text(x), display);
        assert_eq!(debug_text(x), debug);
    }
    assert_eq!(display_text(1.0f64), "1");
    assert_eq!(display_text(-0.0f64), "-0");
    assert_eq!(display_text(1e-7f64), "0.0000001");
    assert_eq!(display_text(1e300f64), format!("{}", 1e300f64));
    assert_eq!(text(|o| _ = o.float_fixed(-0.001, 2)), "-0.00");
    assert_eq!(text(|o| _ = o.float_fixed(0.125, 2)), "0.12");
    assert_eq!(text(|o| _ = o.float_fixed(0.375, 2)), "0.38");
    assert_eq!(text(|o| _ = o.float_fixed(1e300, 1)), format!("{:.1}", 1e300));
    assert_eq!(text(|o| _ = o.float_fixed(5e-324, 1100)), format!("{:.1100}", 5e-324));
}

#[test]
fn random_floats_match_core_fmt_and_read_back() {
    blaze_sim::check(Config::cases(2000), |tape: &Tape| {
        check_f64(f64::from_bits(tape.next_u64("f64 bits")))?;
        check_f64(tape.f64("f64"))?;
        check_f32(f32::from_bits(tape.next_u32("f32 bits")))?;
        check_f32(tape.f32("f32"))
    });
}

#[test]
fn halfway_decimals_round_to_even() {
    // 1 + 2^-53 exactly: halfway between 1 and the next float, so it rounds to 1.
    let half = "1.00000000000000011102230246251565404236316680908203125";
    assert_eq!(parse_all::<f64>(half), Ok(1.0));
    let above = "1.00000000000000011102230246251565404236316680908203126";
    assert_eq!(parse_all::<f64>(above).map(f64::to_bits), Ok(1.0f64.to_bits() + 1));
    // A thousand digits past the 768 kept, nonzero only at the end: the sticky bit.
    let mut long = StdString::from(half);
    long.extend(core::iter::repeat_n('0', 1000));
    long.push('1');
    assert_eq!(parse_all::<f64>(&long).map(f64::to_bits), Ok(1.0f64.to_bits() + 1));
    // The exact oracle on the same cases.
    assert_eq!(exact::<f64>(half), Some(1.0));
    assert_eq!(exact::<f64>(above).map(f64::to_bits), Some(1.0f64.to_bits() + 1));
    assert_eq!(exact::<f64>(&long).map(f64::to_bits), Some(1.0f64.to_bits() + 1));
    for text in ["2.2250738585072012e-308", "4.9406564584124654e-324", "2.4703282292062327e-324", "1e23", "0.1"] {
        assert_eq!(parse_all::<f64>(text).map(f64::to_bits).ok(), text.parse::<f64>().map(f64::to_bits).ok(), "{text}");
    }
}

/// A decimal of `digits` digits, maybe a point, maybe an exponent.
fn decimal_text(tape: &Tape) -> StdString {
    let mut s = StdString::new();
    if tape.coin("negative") {
        s.push('-');
    }
    let digit = |tape: &Tape| char::from(b'0' + u8::try_from(tape.below(10, "digit")).unwrap_or(0));
    for _ in 0..=tape.below(25, "int digits") {
        s.push(digit(tape));
    }
    if tape.coin("point") {
        s.push('.');
        for _ in 0..=tape.below(25, "frac digits") {
            s.push(digit(tape));
        }
    }
    if tape.coin("exponent") {
        s.push('e');
        let e = tape.below(700, "exponent");
        s.push_str(&format!("{}", i64::try_from(e).unwrap_or(0) - 350));
    }
    s
}

/// The whole of `text` through the exact conversion (the oracle `Parse` no longer calls).
fn exact<F: Float>(text: &str) -> Option<F> {
    match crate::parse::parse_float_exact::<F>(text) {
        Ok((x, "")) => Some(x),
        _ => None,
    }
}

#[test]
fn random_decimal_text_reads_as_core_reads_it() {
    blaze_sim::check(Config::cases(4000), |tape: &Tape| {
        let s = decimal_text(tape);
        let core64 = s.parse::<f64>().ok().filter(|x| x.is_finite()).map(f64::to_bits);
        let core32 = s.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| u64::from(x.to_bits()));
        for (got, want) in [
            (parse_all::<f64>(&s).map(f64::to_bits).ok(), core64),
            (exact::<f64>(&s).map(f64::to_bits), core64),
            (parse_all::<f32>(&s).map(|x| u64::from(x.to_bits())).ok(), core32),
            (exact::<f32>(&s).map(|x| u64::from(x.to_bits())), core32),
        ] {
            if got != want {
                return Err(Broken::Oracle(TextBroken::FloatRead { text: s.clone(), got, want }));
            }
        }
        Ok(())
    });
}

// The vector routines against their scalar twins, at every alignment.

/// A buffer of random bytes drawn from `alphabet`, with 64 bytes of slack for offsets.
fn bytes_from(tape: &Tape, alphabet: &[u8], len: usize) -> StdVec<u8> {
    (0..len + 64).map(|_| *tape.one_of(alphabet, "byte")).collect()
}

#[test]
fn vector_routines_equal_their_scalar_twins() {
    blaze_sim::check(Config::cases(500), |tape: &Tape| {
        // Mostly digits, so runs of every length occur.
        let digits = bytes_from(tape, b"0123456789012345678901234567890123456789x-", 40);
        let mixed = bytes_from(tape, b"abc \"\\\n\x01\x7f\xc3\xa9xyz0123", 40);
        for at in 0..32 {
            // The integer reader against core on every window of a digit-heavy buffer.
            let window = &digits[at..];
            let run = window.iter().take_while(|b| b.is_ascii_digit()).count();
            let want = core::str::from_utf8(&window[..run]).ok().and_then(|t| t.parse::<u64>().ok());
            let got = match crate::parse::decimal_u64(window) {
                crate::parse::Decimal64::Value(v, n) if n == run => Some(v),
                crate::parse::Decimal64::Value(..) => return Err(Broken::Oracle(TextBroken::DecimalLength { at })),
                _ => None,
            };
            if got != want {
                return Err(Broken::Oracle(TextBroken::DecimalRead { at, got, want }));
            }
            let window = &mixed[at..];
            for from in [0, 1, 7, 17] {
                let (v, s) = (crate::simd::next_special(window, from), crate::simd::next_special_scalar(window, from));
                if v != s {
                    return Err(Broken::Oracle(TextBroken::NextSpecial { from, at, vector: v, scalar: s }));
                }
            }
            let (v, s) = (crate::simd::hex16(window), crate::simd::hex16_scalar(window));
            if v != s {
                let (vector, scalar) = (format!("{v:?}"), format!("{s:?}"));
                return Err(Broken::Oracle(TextBroken::Hex16 { at, vector, scalar }));
            }
        }
        Ok(())
    });
}

#[test]
fn integer_parse_matches_core_on_random_text() {
    blaze_sim::check(Config::cases(2000), |tape: &Tape| {
        let digits = (tape.u128("value") >> tape.below(128, "shift")).to_string();
        let zeros = StdString::from("000").split_at(usize::try_from(tape.below(4, "zeros")).unwrap_or(0)).0.to_string();
        let tail = if tape.coin("tail") { ", 123456789012345" } else { "" };
        for sign in ["", "-"] {
            let text = format!("{sign}{zeros}{digits}{tail}");
            let token = &text[..text.len() - tail.len()];
            let pairs = [
                (u64::parse(&text).ok().map(|(v, _)| i128::from(v)), token.parse::<u64>().ok().map(i128::from)),
                (i64::parse(&text).ok().map(|(v, _)| i128::from(v)), token.parse::<i64>().ok().map(i128::from)),
                (u32::parse(&text).ok().map(|(v, _)| i128::from(v)), token.parse::<u32>().ok().map(i128::from)),
                (i8::parse(&text).ok().map(|(v, _)| i128::from(v)), token.parse::<i8>().ok().map(i128::from)),
            ];
            let whole = [
                (parse_all::<u64>(token).ok().map(i128::from), token.parse::<u64>().ok().map(i128::from)),
                (parse_all::<i64>(token).ok().map(i128::from), token.parse::<i64>().ok().map(i128::from)),
                (parse_all::<u8>(token).ok().map(i128::from), token.parse::<u8>().ok().map(i128::from)),
            ];
            for (got, want) in pairs.into_iter().chain(whole) {
                if got != want {
                    return Err(Broken::Oracle(TextBroken::IntRead { text: text.clone(), got, want }));
                }
            }
        }
        Ok(())
    });
}

// Parsing.

#[test]
fn parse_stops_and_refuses_at_the_right_offset() {
    assert_eq!(u32::parse("12abc"), Ok((12, "abc")));
    assert_eq!(u32::parse("-5"), Err(ParseError { at: 0, expected: "a digit" }));
    assert_eq!(i32::parse("-"), Err(ParseError { at: 1, expected: "a digit" }));
    assert_eq!(i32::parse("+5"), Err(ParseError { at: 0, expected: "a digit" }));
    assert_eq!(parse_int::<i32>("+5", IntSyntax { radix: 10, plus: true }), Ok((5, "")));
    assert_eq!(u8::parse("256"), Err(ParseError { at: 0, expected: "an integer that fits in u8" }));
    assert_eq!(i8::parse("-128"), Ok((-128, "")));
    assert_eq!(i8::parse("128"), Err(ParseError { at: 0, expected: "an integer that fits in i8" }));
    assert_eq!(u64::parse("18446744073709551615"), Ok((u64::MAX, "")));
    assert_eq!(u64::parse("18446744073709551616").map(|_| ()), Err(ParseError { at: 0, expected: "an integer that fits in u64" }));
    assert_eq!(parse_int::<u32>("ff_", IntSyntax::HEX), Ok((255, "_")));
    assert_eq!(parse_int::<u8>("1012", IntSyntax::BINARY), Ok((5, "2")));
    assert_eq!(parse_all::<u32>("12 "), Err(ParseError { at: 2, expected: "the end of the text" }));
    assert_eq!(bool::parse("truex"), Ok((true, "x")));
    assert_eq!(char::parse("éa"), Ok(('é', "a")));
    assert_eq!(f64::parse("1.5e"), Ok((1.5, "e")));
    assert_eq!(f64::parse("1."), Ok((1.0, ".")));
    assert_eq!(f64::parse("x"), Err(ParseError { at: 0, expected: "a digit" }));
    assert_eq!(f64::parse("1e400").map(|_| ()), Err(ParseError { at: 0, expected: <f64 as Float>::IN_RANGE }));
    assert_eq!(f64::parse("1e-400"), Ok((0.0, "")));
    assert_eq!(f64::parse("-inf,"), Ok((f64::NEG_INFINITY, ",")));
    assert!(f64::parse("NaN").is_ok_and(|(x, _)| x.is_nan()));
}

#[test]
fn quoted_strings_borrow_unless_escaped() {
    assert_eq!(<&str>::parse("\"abc\" rest"), Ok(("abc", " rest")));
    assert_eq!(
        <&str>::parse("\"ab\\nc\""),
        Err(ParseError { at: 3, expected: "a string without escapes (read it with `parse_quoted_in`)" })
    );
    assert_eq!(<&str>::parse("\"abc"), Err(ParseError { at: 4, expected: "the closing `\"`" }));
    match parse_quoted_in("\"abc\"!", heap()) {
        Ok((unquoted, rest)) => {
            assert!(unquoted.is_borrowed());
            assert_eq!((unquoted.as_str(), rest), ("abc", "!"));
        }
        Err(_) => panic!("refused"),
    }
    match parse_quoted_in("\"a\\n\\u{e9}\\x41\\\"z\" tail", heap()) {
        Ok((Unquoted::Owned(owned), rest)) => assert_eq!((owned.as_str(), rest), ("a\néA\"z", " tail")),
        _ => panic!("not unescaped"),
    }
    match parse_quoted_in("\"a\\q\"", heap()) {
        Err(ParseIn::Invalid(error)) => assert_eq!(error.at, 2),
        _ => panic!("a bad escape read"),
    }
    // What `escaped` writes reads back.
    let original = "tab\t \"quote\" \\ nul\0 esc\u{1b} é";
    let written = text(|o| _ = o.escaped(original));
    match parse_quoted_in(&written, heap()) {
        Ok((unquoted, "")) => assert_eq!(unquoted.as_str(), original),
        _ => panic!("{written:?} did not read back"),
    }
}

#[test]
fn combinators_compose() {
    let mut out: Vec<u32, Heap> = Vec::new_in(heap());
    let rest = sep_by("1, 2, 3]", ", ", &mut out, u32::parse);
    assert!(matches!(rest, Ok("]")));
    assert_eq!(out.as_slice(), &[1, 2, 3]);
    let mut out: Vec<u32, Heap> = Vec::new_in(heap());
    match sep_by("1, x", ", ", &mut out, u32::parse) {
        Err(ParseIn::Invalid(error)) => assert_eq!(error, ParseError { at: 3, expected: "a digit" }),
        _ => panic!("accepted"),
    }
    assert_eq!(tag("let x", "let "), Ok("x"));
    assert_eq!(tag("var x", "let "), Err(ParseError { at: 0, expected: "let " }));
    assert_eq!(take_while("abc123", |c| c.is_ascii_alphabetic()), ("abc", "123"));
    assert_eq!(opt("x", u32::parse), (None, "x"));
    assert_eq!(opt("7x", u32::parse), (Some(7), "x"));
}

// Sinks.

#[test]
fn fixed_string_refuses_at_its_bound_and_stays_refused() {
    let mut buf = FixedString::<4>::new();
    buf.str("abc");
    assert_eq!(buf.text(), Ok("abc"));
    buf.str("de");
    assert_eq!(buf.text(), Err(Overflow { capacity: 4, needed: 5 }));
    // Room for one more byte, but the text is already cut: refused too.
    buf.byte(b'x');
    assert_eq!(buf.text(), Err(Overflow { capacity: 4, needed: 6 }));
    buf.clear();
    buf.int(1234u32);
    assert_eq!(buf.text(), Ok("1234"));
    let mut count = Count::new();
    count.str("héllo").int(42u8);
    assert_eq!((count.bytes, count.chars), (8, 7));
}

// Escapes, human units, Show.

#[test]
fn rust_escapes_match_debug_on_ascii() {
    blaze_sim::check(Config::cases(500), |tape: &Tape| {
        let bytes: StdVec<u8> = (0..tape.below(40, "len")).map(|_| u8::try_from(tape.below(128, "byte")).unwrap_or(0)).collect();
        let s = StdString::from_utf8(bytes).map_err(|e| Broken::Harness(format!("{e}")))?;
        same("escaped", &s, text(|o| _ = o.escaped(&s)), format!("{s:?}"))
    });
}

#[test]
fn json_escapes() {
    assert_eq!(text(|o| _ = o.json_str("a\"b\\c\n\u{1}\u{8}\u{7f}é")), "\"a\\\"b\\\\c\\n\\u0001\\b\u{7f}é\"");
    assert_eq!(text(|o| _ = o.hex_bytes(&[0, 0xab, 0x10])), "00ab10");
}

#[test]
fn human_units() {
    let duration = |ns: u64| text(|o| _ = o.duration(ns));
    assert_eq!(duration(850), "850ns");
    assert_eq!(duration(1_250), "1.25µs");
    assert_eq!(duration(1_005_000), "1.00ms");
    assert_eq!(duration(12_560_000), "12.5ms");
    assert_eq!(duration(999_999_999), "999ms");
    assert_eq!(duration(1_500_000_000), "1.50s");
    assert_eq!(duration(59_990_000_000), "59.9s");
    assert_eq!(duration(270_000_000_000), "4m30s");
    assert_eq!(duration(7_500_000_000_000), "2h05m");
    let bytes = |n: u64| text(|o| _ = o.byte_count(n));
    assert_eq!(bytes(512), "512 B");
    assert_eq!(bytes(1024), "1.00 KiB");
    assert_eq!(bytes(1536), "1.50 KiB");
    assert_eq!(bytes(12_900_000), "12.3 MiB");
    assert_eq!(bytes(u64::MAX), "15.9 EiB");
}

#[test]
fn show_and_columns() {
    let mut v: Vec<u32, Heap> = Vec::new_in(heap());
    for x in [1, 2, 3] {
        v.push(x);
    }
    assert_eq!(text(|o| _ = o.show(&v)), "[1, 2, 3]");
    assert_eq!(text(|o| _ = o.show(&Some(4u8)).str(" ").show(&None::<u8>)), "4 none");
    assert_eq!(text(|o| _ = o.left(&"name", 8).right(&Fixed(2.5f64, 1), 6).byte(b'|')), "name       2.5|");
    assert_eq!(format!("{}", Shown(ParseError { at: 3, expected: "a digit" })), "expected a digit at byte 3");
    assert_eq!(format!("[{:^7}]", Shown("ab")), "[  ab   ]");
    assert_eq!(text(|o| _ = o.repeat('-', 20).repeat('é', 2)), "--------------------éé");
}

// The debug form read by reflection, against `#[derive(Debug)]` and `core::fmt`'s `{:?}`.

#[derive(Debug, Clone, Copy)]
struct Point {
    x: i32,
    y: f64,
}

#[derive(Debug)]
struct Pair(u8, &'static str);

#[derive(Debug)]
struct Unit;

#[derive(Debug)]
struct Generic<T> {
    inner: T,
}

#[derive(Debug)]
enum Shape {
    Empty,
    Dot(Point),
    Line { from: Point, to: Point, label: Option<&'static str> },
    Many(u16, i64, char),
}

/// An enum's own form, as every enum but `Option` and `Result` needs one (the debug module
/// says why); its fields go through the reflected walk.
impl crate::DebugForm for Shape {
    fn debug_form(&self, out: &mut dyn Put) {
        match self {
            Shape::Empty => _ = out.str("Empty"),
            Shape::Dot(p) => _ = out.str("Dot(").debug(p).byte(b')'),
            Shape::Line { from, to, label } => {
                _ = out.str("Line { from: ").debug(from).str(", to: ").debug(to).str(", label: ").debug(label).str(" }");
            }
            Shape::Many(a, b, c) => _ = out.str("Many(").debug(a).str(", ").debug(b).str(", ").debug(c).byte(b')'),
        }
    }
}

/// Discriminants set by hand, in the other order: what the walk would name wrongly from the
/// discriminant.
#[derive(Debug, Clone, Copy)]
enum Swapped {
    A = 1,
    B = 0,
}

debug_variants!(Swapped { A, B });

#[test]
fn an_enum_with_set_discriminants_is_named_right_or_not_built() {
    // The walk reads no enum but `Option` and `Result`: any other would stop the build.
    assert!(!const { crate::debug::walks_enum::<Swapped>() });
    assert!(!const { crate::debug::walks_enum::<Shape>() });
    assert!(const { crate::debug::walks_enum::<Option<Swapped>>() });    // Its own form from the one line names each variant right.
    for x in [Swapped::A, Swapped::B, Swapped::B] {
        debug_oracle(&x).unwrap_or_else(|broke| panic!("{broke:?}"));
        debug_oracle(&Some(x)).unwrap_or_else(|broke| panic!("{broke:?}"));
    }
    for x in [core::cmp::Ordering::Less, core::cmp::Ordering::Equal, core::cmp::Ordering::Greater] {
        debug_oracle(&x).unwrap_or_else(|broke| panic!("{broke:?}"));
    }
}

#[derive(Debug)]
struct Everything {
    shape: Shape,
    small: (u8,),
    tuple: (bool, u128, i8),
    unit: (),
    array: [u16; 3],
    empty: [u8; 0],
    slice: &'static [i16],
    text: &'static str,
    nested: Option<Option<bool>>,
    result: Result<u32, Pair>,
    boxed_ref: &'static Unit,
    generic: Generic<Option<&'static str>>,
    big: i128,
    float: f32,
}

const SHAPES: [Shape; 4] = [
    Shape::Empty,
    Shape::Dot(Point { x: -3, y: 0.5 }),
    Shape::Line { from: Point { x: 1, y: 1e20 }, to: Point { x: i32::MIN, y: -0.0 }, label: Some("a \"b\"\n\t\\") },
    Shape::Many(65_535, i64::MIN, '\''),
];

fn debug_oracle<T: core::fmt::Debug>(x: &T) -> Result<(), Broken<TextBroken>> {
    debug_oracle_planted(x, NoPlant)
}

/// The debug form against `{:?}`; the walk takes `plant` ([`NoPlant`] for the clean run).
fn debug_oracle_planted<T: core::fmt::Debug>(x: &T, plant: impl Planted<TextPlant>) -> Result<(), Broken<TextBroken>> {
    let ours = text(|o| crate::debug::debug_planted(x, o, plant));
    let core = format!("{x:?}");
    match ours == core {
        true => Ok(()),
        false => Err(Broken::Oracle(TextBroken::Debug { ours, core })),
    }
}

fn everything(tape: &Tape) -> Everything {
    static SLICES: [&[i16]; 3] = [&[], &[-1], &[1, -32768, 32767]];
    static TEXTS: [&str; 9] = [
        "",
        "plain",
        "quote \" and 'tick'",
        "tab\tnul\0bell\u{7}del\u{7f}",
        "é ünïcode",
        "\u{301}accent first, then e\u{301}",
        "zero\u{200b}width and \u{feff}bom",
        "c1 \u{85} and \u{a0} nbsp",
        "private \u{e000} and unassigned \u{378}",
    ];
    static CHARS: [char; 11] = ['a', '\'', '"', '\n', '\u{1b}', 'é', '\u{301}', '\u{200b}', '\u{85}', '\u{378}', '\u{1f600}'];
    let shape = match tape.below(4, "shape") {
        0 => Shape::Empty,
        1 => Shape::Dot(Point { x: tape.next_u32("x").cast_signed(), y: tape.f64("y") }),
        2 => Shape::Line {
            from: Point { x: 0, y: tape.f64("from") },
            to: Point { x: -1, y: f64::from_bits(tape.next_u64("to")) },
            label: match tape.coin("label") {
                true => Some(*tape.one_of(&TEXTS, "label")),
                false => None,
            },
        },
        _ => Shape::Many(
            u16::try_from(tape.below(65_536, "u16")).unwrap_or(0),
            tape.next_u64("i64").cast_signed(),
            *tape.one_of(&CHARS, "char"),
        ),
    };
    Everything {
        shape,
        small: (u8::try_from(tape.below(256, "u8")).unwrap_or(0),),
        tuple: (tape.coin("bool"), tape.u128("u128"), i8::try_from(tape.below(256, "i8")).unwrap_or(0).wrapping_sub(0)),
        unit: (),
        array: [1, 2, u16::try_from(tape.below(65_536, "u16")).unwrap_or(0)],
        empty: [],
        slice: tape.one_of(&SLICES, "slice"),
        text: tape.one_of(&TEXTS, "text"),
        nested: match tape.below(3, "nested") {
            0 => None,
            1 => Some(None),
            _ => Some(Some(tape.coin("inner"))),
        },
        result: match tape.coin("ok") {
            true => Ok(tape.next_u32("ok")),
            false => Err(Pair(7, tape.one_of(&TEXTS, "pair"))),
        },
        boxed_ref: &Unit,
        generic: Generic { inner: tape.coin("generic").then_some("g") },
        big: tape.u128("i128").cast_signed(),
        float: f32::from_bits(tape.next_u32("f32")),
    }
}

#[test]
fn debug_form_matches_derive_debug() {
    for shape in &SHAPES {
        debug_oracle(shape).unwrap_or_else(|broke| panic!("{broke:?}"));
    }
    
    debug_oracle(&(1u8, "a")).unwrap_or_else(|broke| panic!("{broke:?}"));
    debug_oracle(&()).unwrap_or_else(|broke| panic!("{broke:?}"));
    debug_oracle(&Unit).unwrap_or_else(|broke| panic!("{broke:?}"));
    debug_oracle(&Some(&5u64)).unwrap_or_else(|broke| panic!("{broke:?}"));
    debug_oracle(&core::marker::PhantomData::<u32>).unwrap_or_else(|broke| panic!("{broke:?}"));
    
    blaze_sim::check(Config::cases(1000), |tape: &Tape| debug_oracle(&everything(tape)));
}

blaze_sim::plants! {
    /// The bug planted in the debug walk, with the invariant of the debug oracle that must catch
    /// it.
    enum DebugControl breaks TextBroken {
        /// The walk as it is.
        None,
        /// Every named field written as `field`: the form is not `{:?}`'s.
        DebugFieldNameLost => TextBroken::Debug { .. },
    }
}

impl DebugControl {
    /// The crate's plant this control plants.
    fn planted(self) -> TextPlant {
        match self {
            DebugControl::None => TextPlant::None,
            DebugControl::DebugFieldNameLost => TextPlant::DebugFieldNameLost,
        }
    }
}

#[test]
fn every_debug_plant_is_caught_by_its_invariant() {
    controls(Config::cases(50), |tape, plant: DebugControl| {
        debug_oracle_planted(&everything(tape), plant.planted())
    });
}

struct Celsius(f64);

impl crate::DebugForm for Celsius {
    fn debug_form(&self, out: &mut dyn Put) {
        out.float(self.0).str(" °C");
    }
}

#[test]
fn debug_form_takes_a_types_own_and_owned_collections() {
    #[derive(Debug)]
    struct Reading {
        at: u32,
    }
    let mut v: Vec<Reading, Heap> = Vec::new_in(heap());
    v.push(Reading { at: 1 });
    v.push(Reading { at: 2 });
    let mut s = String::new_in(heap());
    s.push_str("hi\n");
    assert_eq!(text(|o| _ = o.debug(&v)), "[Reading { at: 1 }, Reading { at: 2 }]");
    assert_eq!(text(|o| _ = o.debug(&s)), "\"hi\\n\"");
    assert_eq!(text(|o| _ = o.debug(&(Celsius(21.5), 3u8))), "(21.5 °C, 3)");
    assert_eq!(text(|o| _ = o.debug(&Some(Celsius(-1.0)))), "Some(-1 °C)");
    assert_eq!(text(|o| _ = o.debug("str")), "\"str\"");
    assert_eq!(text(|o| _ = o.debug(&[1u8, 2][..])), "[1, 2]");
    assert_eq!(text(|o| _ = o.show(&crate::Dbg(&Some(1.0f64)))), "Some(1.0)");
}

// One expression per format site: `text_in`, `text_exact_in`, `show_with`.

#[test]
fn text_in_and_show_with_match_format() {
    assert_eq!(size_of_val(&crate::show_with(|out| _ = out.str("static"))), 0);
    blaze_sim::check(Config::cases(500), |tape: &Tape| {
        let n = tape.next_u64("n");
        let x = tape.f64("x");
        let name = *tape.one_of(&["", "a", "naïve", "tab\t"], "name");
        let want = format!("{name}: {n} of {x} ({:?})", name);
        let grown = crate::text_in(heap(), |out| out.str(name).str(": ").int(n).str(" of ").float(x).str(" (").escaped(name).byte(b')'));
        let exact = crate::text_exact_in(heap(), |out| out.str(name).str(": ").int(n).str(" of ").float(x).str(" (").escaped(name).byte(b')'));
        let later = crate::show_with(|out| _ = out.str(name).str(": ").int(n).str(" of ").float(x).str(" (").escaped(name).byte(b')'));
        let shown = text(|o| _ = o.show(&later));
        for (what, got) in [("text_in", grown.as_str()), ("text_exact_in", exact.as_str()), ("show_with", shown.as_str())] {
            if got != want {
                let (value, got, want) = (format!("{n}, {x}, {name:?}"), StdString::from(got), want.clone());
                return Err(Broken::Oracle(TextBroken::Wrote { what, value, got, want }));
            }
        }
        if exact.capacity() != exact.len() {
            return Err(Broken::Oracle(TextBroken::NotExact { capacity: exact.capacity(), len: exact.len() }));
        }
        Ok(())
    });
}

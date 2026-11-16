//! Every writer against `core::fmt` (boundary and random values), with a planted-bug control
//! for the two-digit table; floats against `{:?}` and `{:.N}` on random bits and the known
//! hard cases, and read back; random decimal text against `str::parse`; parse offsets; the
//! fixed buffer's bound; escapes; human units; the adapter.

use std::{
    alloc::Global,
    fmt::{Binary, Display, LowerHex, UpperHex},
    format,
    string::{String as StdString, ToString},
    vec::Vec as StdVec,
};

use blaze_own::{FixedString, Overflow, ParseIn, String, Vec};
use blaze_sim::{Broke, Config, Tape};

use crate::{
    Count, Fixed, Float, IntSyntax, Integer, Parse, ParseError, Put, Show, Shown, Unquoted, Writes,
    parse::{opt, sep_by, tag, take_while},
    parse_all, parse_int, parse_quoted_in,
    plant::{self, Plant},
};

fn text(write: impl FnOnce(&mut String<Global>)) -> StdString {
    let mut out = String::new_in(Global);
    write(&mut out);
    StdString::from(out.as_str())
}

fn same(what: &str, shown: &dyn Display, got: StdString, want: StdString) -> Result<(), Broke> {
    match got == want {
        true => Ok(()),
        false => Err(Broke::Oracle(format!("{what} of {shown}: blaze-text wrote {got:?}, core::fmt {want:?}"))),
    }
}

// Integers.

fn one_int<I: Integer + Show + Display + LowerHex + UpperHex + Binary>(x: I) -> Result<(), Broke> {
    let full = crate::int::nibbles::<I>();
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
    ($tape:ident; $($t:ident)*) => {$(
        // The boundaries: MIN, MAX, and each power of ten and its neighbours.
        for x in [$t::MIN, $t::MAX, $t::MIN.wrapping_add(1), $t::MAX.wrapping_sub(1)] {
            one_int(x)?;
        }
        let ten = $t::try_from(10u8).unwrap_or($t::MAX);
        let mut power = Some(ten);
        while let Some(p) = power {
            for x in [p.wrapping_sub(1), p, p.wrapping_add(1)] {
                one_int(x)?;
                one_int(x.wrapping_neg())?;
            }
            power = p.checked_mul(ten);
        }
        // Random values of every length.
        for _ in 0..8 {
            let bits = $tape.u128("bits") >> $tape.below(128, "shift");
            one_int(from_bits!($t, bits))?;
        }
    )*};
}

fn int_oracle(tape: &Tape) -> Result<(), Broke> {
    ints!(tape; u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize);
    Ok(())
}

#[test]
fn every_integer_writer_matches_core_fmt() {
    blaze_sim::check(Config::cases(200), int_oracle);
}

#[test]
fn the_oracle_catches_a_digit_pair_off_by_one() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || int_oracle(tape));
    blaze_sim::control(Config::cases(50), Plant::Clean, Plant::DigitPairOffByOne, "blaze-text wrote", run);
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

fn float_text<F: Float>(x: F) -> StdString {
    text(|o| _ = o.float(x))
}

fn check_f64(x: f64) -> Result<(), Broke> {
    same("shortest f64", &x, float_text(x), format!("{x:?}"))?;
    if x.is_finite() {
        let written = float_text(x);
        match parse_all::<f64>(&written) {
            Ok(back) if back.to_bits() == x.to_bits() => {}
            other => return Err(Broke::Oracle(format!("{x:?} wrote {written:?}, which read back as {other:?}"))),
        }
    }
    let places = [0usize, 1, 2, 3, 6, 17, 20, 30];
    for p in places {
        same("fixed f64", &x, text(|o| _ = o.float_fixed(x, p)), format!("{x:.p$}"))?;
    }
    Ok(())
}

fn check_f32(x: f32) -> Result<(), Broke> {
    same("shortest f32", &x, float_text(x), format!("{x:?}"))?;
    if x.is_finite() {
        let written = float_text(x);
        match parse_all::<f32>(&written) {
            Ok(back) if back.to_bits() == x.to_bits() => {}
            other => return Err(Broke::Oracle(format!("{x:?} wrote {written:?}, which read back as {other:?}"))),
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
    let specials = [(f64::NAN, "NaN"), (f64::INFINITY, "inf"), (f64::NEG_INFINITY, "-inf"), (0.0, "0.0")];
    for (x, want) in specials {
        assert_eq!(float_text(x), want);
    }
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
                return Err(Broke::Oracle(format!("{s:?} read as bits {got:?}; core reads {want:?}")));
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
                crate::parse::Decimal64::Value(..) => return Err(Broke::Oracle(format!("decimal_u64 at {at}: wrong length"))),
                _ => None,
            };
            if got != want {
                return Err(Broke::Oracle(format!("decimal_u64 at {at} of {window:?}: read {got:?}, core {want:?}")));
            }
            let window = &mixed[at..];
            for from in [0, 1, 7, 17] {
                let (v, s) = (crate::simd::next_special(window, from), crate::simd::next_special_scalar(window, from));
                if v != s {
                    return Err(Broke::Oracle(format!("next_special from {from} at {at}: vector {v}, scalar {s}")));
                }
            }
            let (v, s) = (crate::simd::hex16(window), crate::simd::hex16_scalar(window));
            if v != s {
                return Err(Broke::Oracle(format!("hex16 at {at}: vector {v:?}, scalar {s:?}")));
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
                    return Err(Broke::Oracle(format!("{text:?}: blaze-text read {got:?}, core {want:?}")));
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
    match parse_quoted_in("\"abc\"!", Global) {
        Ok((unquoted, rest)) => {
            assert!(unquoted.is_borrowed());
            assert_eq!((unquoted.as_str(), rest), ("abc", "!"));
        }
        Err(_) => panic!("refused"),
    }
    match parse_quoted_in("\"a\\n\\u{e9}\\x41\\\"z\" tail", Global) {
        Ok((Unquoted::Owned(owned), rest)) => assert_eq!((owned.as_str(), rest), ("a\néA\"z", " tail")),
        _ => panic!("not unescaped"),
    }
    match parse_quoted_in("\"a\\q\"", Global) {
        Err(ParseIn::Invalid(error)) => assert_eq!(error.at, 2),
        _ => panic!("a bad escape read"),
    }
    // What `escaped` writes reads back.
    let original = "tab\t \"quote\" \\ nul\0 esc\u{1b} é";
    let written = text(|o| _ = o.escaped(original));
    match parse_quoted_in(&written, Global) {
        Ok((unquoted, "")) => assert_eq!(unquoted.as_str(), original),
        _ => panic!("{written:?} did not read back"),
    }
}

#[test]
fn combinators_compose() {
    let mut out: Vec<u32, Global> = Vec::new_in(Global);
    let rest = sep_by("1, 2, 3]", ", ", &mut out, u32::parse);
    assert!(matches!(rest, Ok("]")));
    assert_eq!(out.as_slice(), &[1, 2, 3]);
    let mut out: Vec<u32, Global> = Vec::new_in(Global);
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
        let s = StdString::from_utf8(bytes).map_err(|e| Broke::Harness(format!("{e}")))?;
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
    let mut v: Vec<u32, Global> = Vec::new_in(Global);
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

//! [`Parse<'a>`]: a value from the front of a text, and the rest of the text. The value may
//! borrow from the text (a quoted string with no escape is a `&'a str` into it), which
//! `FromStr::from_str(&str) -> Self` cannot say.
//!
//! A parse reads a prefix: `u32::parse("12abc")` is `(12, "abc")`. [`parse_all`] wants the
//! whole text. A [`ParseError`] names the byte offset in the text it was given and what was
//! expected there; combinators that parse a suffix shift it, so offsets stay the caller's.
//!
//! Where these differ from Rust's `FromStr`, and why:
//!
//! - Integers: no leading `+` unless [`IntSyntax::plus`] asks (one spelling per value; the
//!   writers never write one); leading zeros are read, as Rust reads them. Overflow is its own
//!   error naming the type.
//! - Floats: a digit is required before the point and after it (`.5` and `5.` stop before the
//!   `.`), `+` is refused, and only the writers' spellings of the non-finite values are read
//!   (`inf`, `-inf`, `NaN`, case as written), where Rust reads `infinity` and any case. A
//!   value past the type's range is an error naming the range, where Rust gives infinity;
//!   one below the least subnormal is zero, as IEEE rounding says.
//! - `char` is the text's first character, raw.

use core::alloc::Allocator;

use blaze_own::{ParseIn, String, Vec};

use crate::float::parse::{EXP_CLAMP, Number};
use crate::float::Float;

/// Where a text stopped parsing and what was wanted there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// The byte offset in the text the parse was given.
    pub at: usize,
    pub expected: &'static str,
}

impl ParseError {
    /// The same error in a text that had `consumed` bytes before this one.
    #[inline]
    pub const fn after(self, consumed: usize) -> ParseError {
        ParseError { at: self.at + consumed, expected: self.expected }
    }
}

/// A value read from the front of a text.
pub trait Parse<'a>: Sized {
    /// The value and the rest of `text`.
    fn parse(text: &'a str) -> Result<(Self, &'a str), ParseError>;

    /// The whole of `text`, nothing after it ([`parse_all`]). A type overrides it when
    /// knowing the token's end saves work (floats skip their own scan).
    #[inline]
    fn parse_whole(text: &'a str) -> Result<Self, ParseError> {
        let (value, rest) = Self::parse(text)?;
        match rest.is_empty() {
            true => Ok(value),
            false => Err(ParseError { at: consumed(text, rest), expected: "the end of the text" }),
        }
    }
}

/// The bytes of `whole` before `rest`, which is a suffix of it.
#[inline]
fn consumed(whole: &str, rest: &str) -> usize {
    whole.len() - rest.len()
}

/// The whole of `text` as a `T`: nothing may follow it.
#[inline]
pub fn parse_all<'a, T: Parse<'a>>(text: &'a str) -> Result<T, ParseError> {
    T::parse_whole(text)
}

/// How an integer is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntSyntax {
    /// 2 to 36; digits past 9 are letters of either case.
    pub radix: u8,
    /// Whether a leading `+` is read.
    pub plus: bool,
}

impl IntSyntax {
    pub const DECIMAL: IntSyntax = IntSyntax { radix: 10, plus: false };
    pub const HEX: IntSyntax = IntSyntax { radix: 16, plus: false };
    pub const BINARY: IntSyntax = IntSyntax { radix: 2, plus: false };
}

/// An integer that [`parse_int`] reads.
pub trait ParseInt: Sized + crate::int::Integer {
    #[doc(hidden)]
    fn parse_with(text: &str, syntax: IntSyntax) -> Result<(Self, &str), ParseError>;
}

/// An integer written as `syntax` says, from the front of `text`.
///
/// # Panics
///
/// When `syntax.radix` is outside 2 to 36: a bug at the caller.
#[inline]
pub fn parse_int<T: ParseInt>(text: &str, syntax: IntSyntax) -> Result<(T, &str), ParseError> {
    T::parse_with(text, syntax)
}

#[inline(always)]
fn digit_value(b: u8, radix: u8) -> Option<u8> {
    let d = match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'z' => b - b'a' + 10,
        b'A'..=b'Z' => b - b'A' + 10,
        _ => return None,
    };
    if d < radix { Some(d) } else { None }
}

/// The eight ASCII bytes of `chunk` (first digit first) as their decimal value, or `None`
/// when one is not a digit. The SWAR form of simdjson and fast_float: one mask test for all
/// eight, then three multiplies that fold pairs, quads and the two halves.
#[inline(always)]
fn eight_digits(chunk: &[u8]) -> Option<u64> {
    let mut word = [0u8; 8];
    word.copy_from_slice(chunk);
    let w = u64::from_le_bytes(word);
    // A byte below '0' borrows into its high bit; one above '9' carries into it.
    let below = w.wrapping_sub(0x3030_3030_3030_3030);
    let above = w.wrapping_add(0x4646_4646_4646_4646);
    if (below | above) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    let v = (w & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
    let v = (v & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
    Some((v & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32)
}

/// What [`decimal_u64`] read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decimal64 {
    NoDigit,
    Over,
    /// The value and the bytes its digits took.
    Value(u64, usize),
}

/// The decimal digits at the front of `bytes` as a `u64`.
///
/// Most numbers are short, so the first eight digits are a plain byte loop (measured: a
/// padded word load and mask, or a vector compare, cost more than it below eight digits,
/// and the vector run lost at 10 and 20 digits too, so integers use none). Past eight, the
/// SWAR word converts eight more at a time, then the byte loop ends the run. Overflow is
/// decided once at the end: none below 20 significant digits; at exactly 20, by comparing
/// them with `u64::MAX`'s.
#[inline]
pub(crate) fn decimal_u64(bytes: &[u8]) -> Decimal64 {
    let mut acc = 0u64;
    let mut i = 0;
    while i < 8 {
        match bytes.get(i) {
            Some(&b) if b.is_ascii_digit() => {
                acc = acc * 10 + u64::from(b - b'0');
                i += 1;
            }
            _ => {
                return match i {
                    0 => Decimal64::NoDigit,
                    _ => Decimal64::Value(acc, i),
                };
            }
        }
    }
    while let Some(chunk) = bytes.get(i..i + 8) {
        match eight_digits(chunk) {
            Some(v) => acc = acc.wrapping_mul(100_000_000).wrapping_add(v),
            None => break,
        }
        i += 8;
    }
    while let Some(&b) = bytes.get(i) {
        if !b.is_ascii_digit() {
            break;
        }
        acc = acc.wrapping_mul(10).wrapping_add(u64::from(b - b'0'));
        i += 1;
    }
    if i < 20 {
        return Decimal64::Value(acc, i);
    }
    // Twenty digits or more: leading zeros do not count.
    let significant = bytes[..i].iter().take_while(|&&b| b == b'0').count();
    match i - significant {
        0..20 => Decimal64::Value(acc, i),
        20 if bytes[significant..i] <= b"18446744073709551615"[..] => Decimal64::Value(acc, i),
        _ => Decimal64::Over,
    }
}

macro_rules! parse_ints {
    ($($t:ident $u:ident $signed:tt $width:tt;)*) => {$(
        impl ParseInt for $t {
            #[inline]
            fn parse_with(text: &str, syntax: IntSyntax) -> Result<($t, &str), ParseError> {
                assert!(
                    (2..=36).contains(&syntax.radix),
                    "blaze_text::parse_int: radix {} is not 2 to 36",
                    syntax.radix
                );
                const OVER: ParseError =
                    ParseError { at: 0, expected: concat!("an integer that fits in ", stringify!($t)) };
                let bytes = text.as_bytes();
                let mut start = 0;
                let mut negative = false;
                match bytes.first() {
                    Some(b'-') if $signed => {
                        negative = true;
                        start = 1;
                    }
                    Some(b'+') if syntax.plus => start = 1,
                    _ => {}
                }
                let (acc, end): ($u, usize) = parse_ints!(@digits $width $u bytes start syntax OVER);
                let value = parse_ints!(@sign $signed $t acc negative OVER);
                Ok((value, &text[end..]))
            }
        }

        impl<'a> Parse<'a> for $t {
            #[inline]
            fn parse(text: &'a str) -> Result<($t, &'a str), ParseError> {
                <$t as ParseInt>::parse_with(text, IntSyntax::DECIMAL)
            }

            /// The text is the token: `core`'s parse reads it straight (the grammars agree
            /// but for a leading `+`, which `core` reads and this one refuses); a text `core`
            /// refuses is this crate's parser's to name the error of.
            #[inline]
            fn parse_whole(text: &'a str) -> Result<$t, ParseError> {
                if text.as_bytes().first() != Some(&b'+')
                    && let Ok(value) = text.parse::<$t>()
                {
                    return Ok(value);
                }
                let (value, rest) = <$t as ParseInt>::parse_with(text, IntSyntax::DECIMAL)?;
                match rest.is_empty() {
                    true => Ok(value),
                    false => Err(ParseError { at: consumed(text, rest), expected: "the end of the text" }),
                }
            }
        }
    )*};
    // Up to 64 bits in decimal: the eight-at-a-time path into a `u64`, narrowed once.
    (@digits small $u:ident $bytes:ident $start:ident $syntax:ident $over:ident) => {{
        match $syntax.radix {
            10 => match decimal_u64(&$bytes[$start..]) {
                Decimal64::NoDigit => return Err(ParseError { at: $start, expected: "a digit" }),
                Decimal64::Over => return Err($over),
                Decimal64::Value(v, len) => match $u::try_from(v) {
                    Ok(v) => (v, $start + len),
                    Err(_) => return Err($over),
                },
            },
            _ => checked_digits!($u $bytes $start $syntax $over),
        }
    }};
    (@digits wide $u:ident $bytes:ident $start:ident $syntax:ident $over:ident) => {{
        checked_digits!($u $bytes $start $syntax $over)
    }};
    (@sign true $t:ident $acc:ident $negative:ident $over:ident) => {{
        let zero: $t = 0;
        let value = match $negative {
            true => zero.checked_sub_unsigned($acc),
            false => zero.checked_add_unsigned($acc),
        };
        match value {
            Some(it) => it,
            None => return Err($over),
        }
    }};
    (@sign false $t:ident $acc:ident $negative:ident $over:ident) => {{
        let _ = $negative;
        $acc
    }};
}

/// Digits in any radix, each step checked: `(value, end)`.
macro_rules! checked_digits {
    ($u:ident $bytes:ident $start:ident $syntax:ident $over:ident) => {{
        let radix = $u::from($syntax.radix);
        let mut acc: $u = 0;
        let mut i = $start;
        while let Some(d) = $bytes.get(i).and_then(|&b| digit_value(b, $syntax.radix)) {
            acc = match acc.checked_mul(radix).and_then(|it| it.checked_add($u::from(d))) {
                Some(it) => it,
                None => return Err($over),
            };
            i += 1;
        }
        if i == $start {
            return Err(ParseError { at: $start, expected: "a digit" });
        }
        (acc, i)
    }};
}

parse_ints! {
    u8 u8 false small;
    u16 u16 false small;
    u32 u32 false small;
    u64 u64 false small;
    u128 u128 false wide;
    usize usize false small;
    i8 u8 true small;
    i16 u16 true small;
    i32 u32 true small;
    i64 u64 true small;
    i128 u128 true wide;
    isize usize true small;
}

impl<'a> Parse<'a> for bool {
    fn parse(text: &'a str) -> Result<(bool, &'a str), ParseError> {
        if let Some(rest) = text.strip_prefix("true") {
            return Ok((true, rest));
        }
        if let Some(rest) = text.strip_prefix("false") {
            return Ok((false, rest));
        }
        Err(ParseError { at: 0, expected: "`true` or `false`" })
    }
}

impl<'a> Parse<'a> for char {
    fn parse(text: &'a str) -> Result<(char, &'a str), ParseError> {
        let mut chars = text.chars();
        match chars.next() {
            Some(c) => Ok((c, chars.as_str())),
            None => Err(ParseError { at: 0, expected: "a character" }),
        }
    }
}

/// A float from the front of `text`: the token's extent from [`scan_float`], its value from
/// `core`'s correctly rounded `str::parse` (Eisel-Lemire, then big-decimal: about 13 ns). This
/// crate's own exact conversion ([`crate::float::parse`]) is kept as the tests' oracle until
/// an Eisel-Lemire path of its own is written: alone it is 50 times slower.
fn parse_float<F: Float>(text: &str) -> Result<(F, &str), ParseError> {
    // One pass to the first byte that cannot continue a number; a plain decimal there is
    // `core`'s at once. Anything else (a special, `1-2`, `1.5e`, an error, a value past the
    // range) is the scanner's, which finds the exact token and names the error.
    let bytes = text.as_bytes();
    let end = bytes.iter().position(|&b| !NUMBER_BYTE[usize::from(b)]).unwrap_or(bytes.len());
    let token = &text[..end];
    if plain_float(token.as_bytes())
        && let Some(x) = F::core_parse(token)
    {
        return Ok((x, &text[end..]));
    }
    match scan_float::<F>(text)? {
        Scanned::Special(x, rest) => Ok((x, rest)),
        Scanned::Number(_, end) => match F::core_parse(&text[..end]) {
            Some(x) => Ok((x, &text[end..])),
            None => Err(ParseError { at: 0, expected: F::IN_RANGE }),
        },
    }
}

/// A float through the exact conversion: the oracle.
#[cfg(test)]
pub(crate) fn parse_float_exact<F: Float>(text: &str) -> Result<(F, &str), ParseError> {
    match scan_float::<F>(text)? {
        Scanned::Special(x, rest) => Ok((x, rest)),
        Scanned::Number(number, end) => match crate::float::parse::value::<F>(&number) {
            Some(x) => Ok((x, &text[end..])),
            None => Err(ParseError { at: 0, expected: F::IN_RANGE }),
        },
    }
}

enum Scanned<'a, F> {
    /// `inf`, `-inf` or `NaN`, and the rest.
    Special(F, &'a str),
    /// A finite decimal and the byte its token ends at.
    Number(Number<'a>, usize),
}

/// The extent and parts of a float token at the front of `text`.
fn scan_float<F: Float>(text: &str) -> Result<Scanned<'_, F>, ParseError> {
    let bytes = text.as_bytes();
    let negative = bytes.first() == Some(&b'-');
    let mut i = usize::from(negative);
    for (word, bits) in [("inf", F::INFINITY_BITS), ("NaN", F::NAN_BITS)] {
        if let Some(rest) = text[i..].strip_prefix(word) {
            let sign = match negative && word == "inf" {
                true => 1u64 << (F::MANTISSA_BITS + F::EXPONENT_BITS),
                false => 0,
            };
            if negative && word == "NaN" {
                return Err(ParseError { at: 0, expected: "`NaN` without a sign" });
            }
            return Ok(Scanned::Special(F::from_bits64(bits | sign), rest));
        }
    }
    let digits = |from: usize| bytes[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let int_len = digits(i);
    if int_len == 0 {
        return Err(ParseError { at: i, expected: "a digit" });
    }
    let int = &bytes[i..i + int_len];
    i += int_len;
    let mut frac: &[u8] = &[];
    if bytes.get(i) == Some(&b'.') {
        let len = digits(i + 1);
        if len > 0 {
            frac = &bytes[i + 1..i + 1 + len];
            i += 1 + len;
        }
    }
    let mut exp = 0i64;
    if let Some(b'e' | b'E') = bytes.get(i) {
        let mut j = i + 1;
        let exp_negative = bytes.get(j) == Some(&b'-');
        if matches!(bytes.get(j), Some(b'-' | b'+')) {
            j += 1;
        }
        let len = digits(j);
        if len > 0 {
            for &b in &bytes[j..j + len] {
                if exp < EXP_CLAMP {
                    exp = exp * 10 + i64::from(b - b'0');
                }
            }
            exp = exp.min(EXP_CLAMP);
            if exp_negative {
                exp = -exp;
            }
            i = j + len;
        }
    }
    Ok(Scanned::Number(Number { negative, int, frac, exp }, i))
}

/// The bytes a float token can hold: digits, `.`, `e`, `E`, `+`, `-`.
static NUMBER_BYTE: [bool; 256] = {
    const fn at(b: u8) -> usize {
        blaze_int::Cast::<u8, usize>::widen(b)
    }
    let mut table = [false; 256];
    let mut b = b'0';
    while b <= b'9' {
        table[at(b)] = true;
        b += 1;
    }
    table[at(b'.')] = true;
    table[at(b'e')] = true;
    table[at(b'E')] = true;
    table[at(b'+')] = true;
    table[at(b'-')] = true;
    table
};

/// Whether `text` is a plain decimal in this crate's float grammar as far as `core` could
/// read it differently: a digit first (after one `-`), a digit last, and a digit after every
/// `.` (`core` also reads `+1`, `.5`, `5.`, `1.e5`, `infinity` and any case of `nan`).
#[inline]
fn plain_float(bytes: &[u8]) -> bool {
    let first = usize::from(bytes.first() == Some(&b'-'));
    let digit_at = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    digit_at(first)
        && bytes.last().is_some_and(u8::is_ascii_digit)
        && match bytes.iter().position(|&b| b == b'.') {
            Some(point) => digit_at(point + 1),
            None => true,
        }
}

/// The whole text as a float: a plain decimal goes to `core` straight, with no scan of ours;
/// anything else (a special, an error, a value past the range) is the scanner's to judge,
/// so the error is this crate's. Both give the correctly rounded value, so the answer does
/// not depend on which ran.
#[inline]
fn parse_whole_float<F: Float>(text: &str) -> Result<F, ParseError> {
    if plain_float(text.as_bytes())
        && let Some(x) = F::core_parse(text)
    {
        return Ok(x);
    }
    let (x, rest) = parse_float::<F>(text)?;
    match rest.is_empty() {
        true => Ok(x),
        false => Err(ParseError { at: consumed(text, rest), expected: "the end of the text" }),
    }
}

impl<'a> Parse<'a> for f64 {
    #[inline]
    fn parse(text: &'a str) -> Result<(f64, &'a str), ParseError> {
        parse_float::<f64>(text)
    }

    #[inline]
    fn parse_whole(text: &'a str) -> Result<f64, ParseError> {
        parse_whole_float::<f64>(text)
    }
}

impl<'a> Parse<'a> for f32 {
    #[inline]
    fn parse(text: &'a str) -> Result<(f32, &'a str), ParseError> {
        parse_float::<f32>(text)
    }

    #[inline]
    fn parse_whole(text: &'a str) -> Result<f32, ParseError> {
        parse_whole_float::<f32>(text)
    }
}

/// A quoted string's text: borrowed from the input when it had no escape, else owned.
pub enum Unquoted<'a, A: Allocator> {
    Borrowed(&'a str),
    Owned(String<A>),
}

impl<A: Allocator> Unquoted<'_, A> {
    pub fn as_str(&self) -> &str {
        match self {
            Unquoted::Borrowed(text) => text,
            Unquoted::Owned(text) => text.as_str(),
        }
    }

    pub fn is_borrowed(&self) -> bool {
        matches!(self, Unquoted::Borrowed(_))
    }
}

/// The body of a `"`-quoted string at the front of `text`, up to the closing quote, and the
/// offset of its first `\` if any.
fn quoted_span(text: &str) -> Result<(usize, Option<usize>), ParseError> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return Err(ParseError { at: 0, expected: "`\"`" });
    }
    let mut first_escape = None;
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((i, first_escape)),
            b'\\' => {
                first_escape.get_or_insert(i);
                i += 2;
            }
            _ => i += 1,
        }
    }
    Err(ParseError { at: text.len(), expected: "the closing `\"`" })
}

/// A `"`-quoted string with no escape, borrowed from the text. One with an escape is refused
/// at its `\`: read it with [`parse_quoted_in`].
impl<'a> Parse<'a> for &'a str {
    fn parse(text: &'a str) -> Result<(&'a str, &'a str), ParseError> {
        let (close, escape) = quoted_span(text)?;
        if let Some(at) = escape {
            return Err(ParseError { at, expected: "a string without escapes (read it with `parse_quoted_in`)" });
        }
        Ok((&text[1..close], &text[close + 1..]))
    }
}

/// A `"`-quoted string with Rust's escapes (`\n \r \t \\ \0 \" \' \xNN \u{N..}`, the forms
/// [`Writes::escaped`](crate::Writes::escaped) writes): borrowed when it has none, else
/// unescaped into a [`String`] in `alloc`.
pub fn parse_quoted_in<A: Allocator>(text: &str, alloc: A) -> Result<(Unquoted<'_, A>, &str), ParseIn<ParseError>> {
    let (close, escape) = quoted_span(text).map_err(ParseIn::Invalid)?;
    let rest = &text[close + 1..];
    let Some(first) = escape else {
        return Ok((Unquoted::Borrowed(&text[1..close]), rest));
    };
    let body = &text[..close];
    let mut out = String::try_with_capacity_in(close - 1, alloc).map_err(ParseIn::Refused)?;
    let push = |out: &mut String<A>, piece: &str| out.try_push_str(piece).map_err(ParseIn::Refused);
    push(&mut out, &body[1..first])?;
    let mut i = first;
    let bytes = body.as_bytes();
    while i < body.len() {
        if bytes[i] != b'\\' {
            let next = body[i..].find('\\').map_or(body.len(), |at| i + at);
            push(&mut out, &body[i..next])?;
            i = next;
            continue;
        }
        let bad = |at: usize| ParseIn::Invalid(ParseError { at, expected: "an escape (`\\n \\r \\t \\\\ \\0 \\\" \\' \\xNN \\u{N}`)" });
        let simple = match bytes.get(i + 1) {
            Some(b'n') => Some('\n'),
            Some(b'r') => Some('\r'),
            Some(b't') => Some('\t'),
            Some(b'\\') => Some('\\'),
            Some(b'0') => Some('\0'),
            Some(b'"') => Some('"'),
            Some(b'\'') => Some('\''),
            _ => None,
        };
        if let Some(c) = simple {
            out.try_push(c).map_err(ParseIn::Refused)?;
            i += 2;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'x') => {
                let hex = body.get(i + 2..i + 4).ok_or_else(|| bad(i))?;
                let value = u8::from_str_radix_ascii(hex).filter(|&v| v < 0x80).ok_or_else(|| bad(i))?;
                out.try_push(char::from(value)).map_err(ParseIn::Refused)?;
                i += 4;
            }
            Some(b'u') if bytes.get(i + 2) == Some(&b'{') => {
                let close = body[i + 3..].find('}').ok_or_else(|| bad(i))?;
                let hex = &body[i + 3..i + 3 + close];
                if hex.is_empty() || hex.len() > 6 {
                    return Err(bad(i));
                }
                let (code, rest) = parse_int::<u32>(hex, IntSyntax::HEX).map_err(|_| bad(i))?;
                let c = match rest.is_empty() {
                    true => char::from_u32(code).ok_or_else(|| bad(i))?,
                    false => return Err(bad(i)),
                };
                out.try_push(c).map_err(ParseIn::Refused)?;
                i += 4 + close;
            }
            _ => return Err(bad(i)),
        }
    }
    Ok((Unquoted::Owned(out), rest))
}

/// Two hex digits.
trait FromHexAscii: Sized {
    fn from_str_radix_ascii(text: &str) -> Option<Self>;
}

impl FromHexAscii for u8 {
    fn from_str_radix_ascii(text: &str) -> Option<u8> {
        match parse_int::<u8>(text, IntSyntax::HEX) {
            Ok((value, "")) => Some(value),
            _ => None,
        }
    }
}

// Combinators: plain functions over `(value, rest)`, so a parser reads straight down.

/// `want` at the front of `text`, and the rest.
#[inline]
pub fn tag<'a>(text: &'a str, want: &'static str) -> Result<&'a str, ParseError> {
    text.strip_prefix(want).ok_or(ParseError { at: 0, expected: want })
}

/// The longest prefix of characters `keep` accepts, and the rest.
#[inline]
pub fn take_while<'a>(text: &'a str, keep: impl Fn(char) -> bool) -> (&'a str, &'a str) {
    let end = text.char_indices().find(|&(_, c)| !keep(c)).map_or(text.len(), |(at, _)| at);
    text.split_at(end)
}

/// [`take_while`] that wants at least one character, naming it when there is none.
#[inline]
pub fn take_while1<'a>(
    text: &'a str,
    keep: impl Fn(char) -> bool,
    expected: &'static str,
) -> Result<(&'a str, &'a str), ParseError> {
    match take_while(text, keep) {
        ("", _) => Err(ParseError { at: 0, expected }),
        taken => Ok(taken),
    }
}

/// Spaces, tabs and line ends skipped.
#[inline]
pub fn skip_space(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\n', '\r'])
}

/// `item` when it parses, else `None` and `text` as it was.
#[inline]
pub fn opt<'a, T>(
    text: &'a str,
    item: impl FnOnce(&'a str) -> Result<(T, &'a str), ParseError>,
) -> (Option<T>, &'a str) {
    match item(text) {
        Ok((value, rest)) => (Some(value), rest),
        Err(_) => (None, text),
    }
}

/// Zero or more `item`s separated by `sep`, pushed onto `out`, and the rest. An item that
/// fails after a separator is the error (at its offset in `text`); an allocator that refuses
/// the next element is a [`ParseIn::Refused`]. With blaze-own's `Alloc<'_>` handle this takes
/// `&mut Vec<T, Alloc<'_>>` and is compiled once per `T`, not per allocator.
pub fn sep_by<'a, T, A: Allocator>(
    text: &'a str,
    sep: &'static str,
    out: &mut Vec<T, A>,
    mut item: impl FnMut(&'a str) -> Result<(T, &'a str), ParseError>,
) -> Result<&'a str, ParseIn<ParseError>> {
    let (first, mut rest) = match item(text) {
        Ok(it) => it,
        Err(_) => return Ok(text),
    };
    out.try_push(first).map_err(|refused| ParseIn::Refused(refused.why))?;
    while let Some(after) = rest.strip_prefix(sep) {
        let (value, next) = item(after).map_err(|e| ParseIn::Invalid(e.after(consumed(text, after))))?;
        out.try_push(value).map_err(|refused| ParseIn::Refused(refused.why))?;
        rest = next;
    }
    Ok(rest)
}

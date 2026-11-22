//! The unescaper against the cases rust-lang/literal-escaper documents in its own tests (each
//! error, each warning's range), and a property: a string of values, each written in a random
//! one of the forms Rust allows for it (itself, a simple escape, `\x`, `\u{..}` with leading
//! zeros and underscores) with line continuations between them, reads back as those values with
//! the range each was written in; and a planted-bug control (rule 53), a reader that reports
//! one-byte ranges, which the oracle must catch.

use core::ops::Range;

use blaze_sim::{Broke, Config, Tape};

use super::*;

#[test]
fn char_and_byte_errors_are_rustcs() {
    use EscapeError::*;
    let both = [
        ("", ZeroChars),
        (r"\", LoneSlash),
        ("\n", EscapeOnlyChar),
        ("\t", EscapeOnlyChar),
        ("'", EscapeOnlyChar),
        ("\r", BareCarriageReturn),
        ("spam", MoreThanOneChar),
        (r"\x0ff", MoreThanOneChar),
        (r#"\"a"#, MoreThanOneChar),
        (r"\0a", MoreThanOneChar),
        (r"\v", InvalidEscape),
        (r"\●", InvalidEscape),
        (r"\x", TooShortHexEscape),
        (r"\x0", TooShortHexEscape),
        (r"\xx", InvalidCharInHexEscape),
        (r"\x🦀", InvalidCharInHexEscape),
        (r"\u", NoBraceInUnicodeEscape),
        (r"\u[0123]", NoBraceInUnicodeEscape),
        (r"\u{0x}", InvalidCharInUnicodeEscape),
        (r"\u{", UnclosedUnicodeEscape),
        (r"\u{0000", UnclosedUnicodeEscape),
        (r"\u{}", EmptyUnicodeEscape),
        (r"\u{_0000}", LeadingUnderscoreUnicodeEscape),
        (r"\u{0000000}", OverlongUnicodeEscape),
    ];
    for (text, error) in both {
        assert_eq!(unescape_char(text), Err(error), "char {text:?}");
        assert_eq!(unescape_byte(text), Err(error), "byte {text:?}");
    }
    let chars = [
        ("\\\r", InvalidEscape),
        (r"\u{0}x", MoreThanOneChar),
        (r"\xff", OutOfRangeHexEscape),
        (r"\x80", OutOfRangeHexEscape),
        (r"\u{FFFFFF}", OutOfRangeUnicodeEscape),
        (r"\u{D800}", LoneSurrogateUnicodeEscape),
        (r"\u{DFFF}", LoneSurrogateUnicodeEscape),
    ];
    for (text, error) in chars {
        assert_eq!(unescape_char(text), Err(error), "char {text:?}");
    }
    for (text, error) in [("ы", NonAsciiCharInByte), (r"\u{41}", UnicodeEscapeInByte), (r"\u{D800}", UnicodeEscapeInByte)] {
        assert_eq!(unescape_byte(text), Err(error), "byte {text:?}");
    }
}

#[test]
fn char_and_byte_values_are_rustcs() {
    for (text, c) in [
        ("🦀", '🦀'),
        (r#"\""#, '"'),
        (r"\'", '\''),
        (r"\0", '\0'),
        (r"\x5A", 'Z'),
        (r"\x7f", '\u{7f}'),
        (r"\u{000000}", '\0'),
        (r"\u{4__1__}", 'A'),
        (r"\u{1F63b}", '😻'),
    ] {
        assert_eq!(unescape_char(text), Ok(c), "char {text:?}");
    }
    for (text, b) in [(r"\x80", 0x80), (r"\xFF", 0xFF), (r"\t", b'\t')] {
        assert_eq!(unescape_byte(text), Ok(b), "byte {text:?}");
    }
}

fn all<T>(read: impl FnOnce(&mut dyn FnMut(Range<usize>, Result<T, EscapeError>))) -> Vec<(Range<usize>, Result<T, EscapeError>)> {
    let mut out = Vec::new();
    read(&mut |range, value| out.push((range, value)));
    out
}

#[test]
fn line_continuations_warn_as_rustc_does() {
    use EscapeError::*;
    assert!(all(|f| unescape_str("\\\n", f)).is_empty());
    assert!(all(|f| unescape_str("\\\n ", f)).is_empty());
    assert_eq!(
        all(|f| unescape_str("\\\n \u{a0} x", f)),
        [(0..5, Err(UnskippedWhitespaceWarning)), (3..5, Ok('\u{a0}')), (5..6, Ok(' ')), (6..7, Ok('x'))]
    );
    assert_eq!(all(|f| unescape_str("\\\n  \n  x", f)), [(0..7, Err(MultipleSkippedLinesWarning)), (7..8, Ok('x'))]);
    assert_eq!(all(|f| unescape_byte_str("hello \\\n     w", f)).len(), 7);
    assert!(!MultipleSkippedLinesWarning.is_fatal() && NulInCStr.is_fatal());
}

#[test]
fn raw_and_c_strings_are_rustcs() {
    use EscapeError::*;
    assert_eq!(all(|f| check_raw_str("\rx", f)), [(0..1, Err(BareCarriageReturnInRawString)), (1..2, Ok('x'))]);
    assert_eq!(all(|f| check_raw_byte_str("🦀a", f)), [(0..4, Err(NonAsciiCharInByte)), (4..5, Ok(b'a'))]);
    let high = NonZero::new(0xFFu8).expect("not zero");
    let a = NonZero::new('a').expect("not zero");
    assert_eq!(
        all(|f| unescape_c_str(r"\xFFa\0\x00", f)),
        [(0..4, Ok(MixedUnit::HighByte(high))), (4..5, Ok(MixedUnit::Char(a))), (5..7, Err(NulInCStr)), (7..11, Err(NulInCStr))]
    );
    let mut errors = Vec::new();
    check_for_errors(r"\x80", Mode::ByteStr, |range, e| errors.push((range, e)));
    check_for_errors(r"\x80", Mode::Char, |range, e| errors.push((range, e)));
    check_for_errors("a\r", Mode::RawStr, |range, e| errors.push((range, e)));
    assert_eq!(errors, [(0..4, OutOfRangeHexEscape), (1..2, BareCarriageReturnInRawString)]);
}

/// The values the property writes: ASCII that needs an escape, that has one, that has none, and
/// characters of two, three and four bytes.
const VALUES: &[char] = &['a', '\0', '\n', '\t', '"', '\'', '\\', ' ', '\u{7f}', 'é', '●', '🦀'];

/// One of the forms Rust allows for `c` in a string, drawn.
fn written(tape: &Tape, c: char) -> String {
    let code = u32::from(c);
    let simple = match c {
        '\0' => Some('0'),
        '\n' => Some('n'),
        '\t' => Some('t'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '\\' => Some('\\'),
        _ => None,
    };
    let mut forms = Vec::new();
    if !matches!(c, '"' | '\\') {
        forms.push(String::from(c));
    }
    if let Some(e) = simple {
        forms.push(format!("\\{e}"));
    }
    if code < 0x80 {
        forms.push(format!("\\x{code:02x}"));
        forms.push(format!("\\x{code:02X}"));
    }
    let zeros = tape.index(6 - format!("{code:x}").len() + 1, "leading zeros");
    let underscores = "_".repeat(tape.index(3, "underscores"));
    forms.push(format!("\\u{{{:0width$x}{underscores}}}", code, width = format!("{code:x}").len() + zeros));
    forms.swap_remove(tape.index(forms.len(), "form"))
}

/// Writes a string of drawn values and returns it with each value's range.
fn draw(tape: &Tape) -> (String, Vec<(Range<usize>, Result<char, EscapeError>)>) {
    let mut text = String::new();
    let mut expected = Vec::new();
    for c in tape.vec(0..=12, "values", |tape| VALUES[tape.index(VALUES.len(), "value")]) {
        let form = written(tape, c);
        // A continuation would skip a written space, tab or newline after it: only before `\`.
        if form.starts_with('\\') && tape.one_in(4, "continuation") {
            text.push_str("\\\n");
            text.push_str(&" ".repeat(tape.index(3, "indent")));
        }
        let start = text.len();
        text.push_str(&form);
        expected.push((start..text.len(), Ok(c)));
    }
    (text, expected)
}

#[derive(Clone, Copy, Debug)]
enum Reader {
    Clean,
    /// Reports each value's range as its first byte only.
    OneByteRanges,
}

fn case(tape: &Tape, reader: Reader) -> Result<(), Broke> {
    let (text, expected) = draw(tape);
    let mut read = all(|f| unescape_str(&text, f));
    if let Reader::OneByteRanges = reader {
        for (range, _) in &mut read {
            range.end = range.start + 1;
        }
    }
    if read != expected {
        return Err(Broke::Oracle(format!("{text:?} read as {read:?}, not as written: {expected:?}")));
    }
    Ok(())
}

/// The invariant: every valid Rust escape sequence and line continuation unescapes back to its written value with its exact byte range.
#[test]
fn every_written_form_reads_back_with_its_range() {
    blaze_sim::check(Config::cases(2000), |tape| case(tape, Reader::Clean));
}

#[test]
fn the_oracle_catches_a_reader_with_one_byte_ranges() {
    blaze_sim::control(Config::cases(500), Reader::Clean, Reader::OneByteRanges, "not as written", case);
}

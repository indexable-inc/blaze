//! The shared cooking against the two copies it replaced, kept here as references:
//! blaze-macro-eval's `lex/cook.rs` (every error, fatal ones stopping) and blaze-fmt's
//! `parse/lexer.rs` (the first error, lexing on). blaze-fmt's messages were its own
//! (`invalid literal: {err:?}`, `invalid raw string literal`) and are rustc's now, so against
//! it only the token and whether it has an error are compared.

use hegel::TestCase;
use hegel::generators as gs;
use rustc_lexer::{Base, Cursor, FrontmatterAllowed, LiteralKind, TokenKind};
use rustc_literal_escaper::{Mode, check_for_errors};

use super::*;

/// The first token of `src` if it is a literal: its kind, length and suffix start.
fn literal(src: &str) -> Option<(LiteralKind, u32, u32)> {
    let token = Cursor::new(src, FrontmatterAllowed::No).advance_token();
    match token.kind {
        TokenKind::Literal { kind, suffix_start } => Some((kind, token.len, suffix_start)),
        _ => None,
    }
}

fn cook(src: &str) -> Option<(Result<(LitKind, String), CookError>, Vec<CookError>)> {
    let (kind, len, suffix_start) = literal(src)?;
    let mut errors = Vec::new();
    let cooked = cook_lexer_literal(src, len, suffix_start, kind, |e| errors.push(e));
    Some((cooked.map(|(kind, sym)| (kind, sym.to_owned())), errors))
}

/// blaze-macro-eval's `cook_lexer_literal` as it was, over `src` from 0.
mod macro_eval {
    use super::*;

    pub(super) struct Fatal;

    fn fatal(errors: &mut Vec<String>, m: String) -> Fatal {
        errors.push(m);
        Fatal
    }

    pub(super) fn cook(src: &str, end: u32, kind: LiteralKind, errors: &mut Vec<String>) -> Result<(LitKind, String), Fatal> {
        let str_from_to = |s: u32, e: u32| &src[s as usize..e as usize];
        let unterminated = |errors: &mut Vec<String>, terminated: bool, m: &str| match terminated {
            true => Ok(()),
            false => {
                errors.push(m.to_owned());
                Err(Fatal)
            }
        };
        Ok(match kind {
            LiteralKind::Char { terminated } => {
                unterminated(errors, terminated, "unterminated character literal")?;
                quoted(src, LitKind::Char, Mode::Char, end, 1, 1, errors)
            }
            LiteralKind::Byte { terminated } => {
                unterminated(errors, terminated, "unterminated byte constant")?;
                quoted(src, LitKind::Byte, Mode::Byte, end, 2, 1, errors)
            }
            LiteralKind::Str { terminated } => {
                unterminated(errors, terminated, "unterminated double quote string")?;
                quoted(src, LitKind::Str, Mode::Str, end, 1, 1, errors)
            }
            LiteralKind::ByteStr { terminated } => {
                unterminated(errors, terminated, "unterminated double quote byte string")?;
                quoted(src, LitKind::ByteStr, Mode::ByteStr, end, 2, 1, errors)
            }
            LiteralKind::CStr { terminated } => {
                unterminated(errors, terminated, "unterminated C string")?;
                quoted(src, LitKind::CStr, Mode::CStr, end, 2, 1, errors)
            }
            LiteralKind::RawStr { n_hashes } => match n_hashes {
                Some(n) => quoted(src, LitKind::StrRaw(n), Mode::RawStr, end, 2 + u32::from(n), 1 + u32::from(n), errors),
                None => return Err(fatal(errors, raw_message(src, 1))),
            },
            LiteralKind::RawByteStr { n_hashes } => match n_hashes {
                Some(n) => quoted(src, LitKind::ByteStrRaw(n), Mode::RawByteStr, end, 3 + u32::from(n), 1 + u32::from(n), errors),
                None => return Err(fatal(errors, raw_message(src, 2))),
            },
            LiteralKind::RawCStr { n_hashes } => match n_hashes {
                Some(n) => quoted(src, LitKind::CStrRaw(n), Mode::RawCStr, end, 3 + u32::from(n), 1 + u32::from(n), errors),
                None => return Err(fatal(errors, raw_message(src, 2))),
            },
            LiteralKind::Int { base, empty_int } => {
                let mut kind = LitKind::Integer;
                if empty_int {
                    errors.push("no valid digits found for number".to_owned());
                    kind = LitKind::Err;
                } else if matches!(base, Base::Binary | Base::Octal) {
                    let radix = base as u32;
                    for c in str_from_to(2, end).chars() {
                        if c != '_' && c.to_digit(radix).is_none() {
                            errors.push(format!("invalid digit for a base {radix} literal"));
                            kind = LitKind::Err;
                        }
                    }
                }
                (kind, str_from_to(0, end).to_owned())
            }
            LiteralKind::Float { base, empty_exponent } => {
                let mut kind = LitKind::Float;
                if empty_exponent {
                    errors.push("expected at least one digit in exponent".to_owned());
                    kind = LitKind::Err;
                }
                let base = match base {
                    Base::Hexadecimal => Some("hexadecimal"),
                    Base::Octal => Some("octal"),
                    Base::Binary => Some("binary"),
                    Base::Decimal => None,
                };
                if let Some(base) = base {
                    errors.push(format!("{base} float literal is not supported"));
                    kind = LitKind::Err;
                }
                (kind, str_from_to(0, end).to_owned())
            }
        })
    }

    fn raw_message(src: &str, prefix_len: u32) -> String {
        match rustc_lexer::validate_raw_str(src, prefix_len) {
            Err(RawStrError::InvalidStarter { bad_char }) => {
                let mut message =
                    String::from("found invalid character; only `#` is allowed in raw string delimitation: ");
                escaped_char(bad_char, &mut message);
                message
            }
            Err(RawStrError::NoTerminator { .. }) => "unterminated raw string".to_owned(),
            Err(RawStrError::TooManyDelimiters { found }) => format!(
                "too many `#` symbols: raw strings may be delimited by up to 255 `#` symbols, but found {found}"
            ),
            Ok(()) => panic!("no error found for supposedly invalid raw string literal"),
        }
    }

    fn quoted(
        src: &str,
        mut kind: LitKind,
        mode: Mode,
        end: u32,
        prefix_len: u32,
        postfix_len: u32,
        errors: &mut Vec<String>,
    ) -> (LitKind, String) {
        let lit_content = &src[prefix_len as usize..(end - postfix_len) as usize];
        check_for_errors(lit_content, mode, |range, err| {
            if let Some(message) = escape_message(lit_content, mode, range, &err) {
                errors.push(message);
                kind = LitKind::Err;
            }
        });
        match kind {
            LitKind::Err => (kind, src[..end as usize].to_owned()),
            _ => (kind, lit_content.to_owned()),
        }
    }
}

/// blaze-fmt's `cook_lexer_literal` as it was: the kind, the symbol and whether it reported.
fn fmt_reference(src: &str, end: u32, kind: LiteralKind) -> (LitKind, String, bool) {
    let mut error = false;
    let whole = || src[..end as usize].to_owned();
    let quoted = |error: &mut bool, kind: LitKind, prefix: u32, postfix: u32| {
        let content_start = prefix;
        let content_end = end - postfix;
        if content_end < content_start {
            return (LitKind::Err, whole());
        }
        let content = &src[content_start as usize..content_end as usize];
        let mode = match kind {
            LitKind::Char => Mode::Char,
            LitKind::Byte => Mode::Byte,
            LitKind::Str => Mode::Str,
            LitKind::ByteStr => Mode::ByteStr,
            LitKind::CStr => Mode::CStr,
            LitKind::StrRaw(_) => Mode::RawStr,
            LitKind::ByteStrRaw(_) => Mode::RawByteStr,
            LitKind::CStrRaw(_) => Mode::RawCStr,
            _ => unreachable!(),
        };
        let mut bad = false;
        check_for_errors(content, mode, |_, err| bad |= err.is_fatal());
        if bad {
            *error = true;
            return (LitKind::Err, whole());
        }
        (kind, content.to_owned())
    };
    let (kind, sym) = match kind {
        LiteralKind::Char { terminated } => {
            error |= !terminated;
            quoted(&mut error, LitKind::Char, 1, 1)
        }
        LiteralKind::Byte { terminated } => {
            error |= !terminated;
            quoted(&mut error, LitKind::Byte, 2, 1)
        }
        LiteralKind::Str { terminated } => {
            error |= !terminated;
            quoted(&mut error, LitKind::Str, 1, 1)
        }
        LiteralKind::ByteStr { terminated } => {
            error |= !terminated;
            quoted(&mut error, LitKind::ByteStr, 2, 1)
        }
        LiteralKind::CStr { terminated } => {
            error |= !terminated;
            quoted(&mut error, LitKind::CStr, 2, 1)
        }
        LiteralKind::RawStr { n_hashes: Some(n) } => {
            quoted(&mut error, LitKind::StrRaw(n), 2 + u32::from(n), 1 + u32::from(n))
        }
        LiteralKind::RawByteStr { n_hashes: Some(n) } => {
            quoted(&mut error, LitKind::ByteStrRaw(n), 3 + u32::from(n), 1 + u32::from(n))
        }
        LiteralKind::RawCStr { n_hashes: Some(n) } => {
            quoted(&mut error, LitKind::CStrRaw(n), 3 + u32::from(n), 1 + u32::from(n))
        }
        LiteralKind::RawStr { n_hashes: None }
        | LiteralKind::RawByteStr { n_hashes: None }
        | LiteralKind::RawCStr { n_hashes: None } => {
            error = true;
            (LitKind::Err, whole())
        }
        LiteralKind::Int { base, empty_int } => {
            let mut kind = LitKind::Integer;
            if empty_int {
                error = true;
                kind = LitKind::Err;
            } else if matches!(base, Base::Binary | Base::Octal) {
                let radix = base as u32;
                if src[2..end as usize].chars().any(|c| c != '_' && c.to_digit(radix).is_none()) {
                    error = true;
                    kind = LitKind::Err;
                }
            }
            (kind, whole())
        }
        LiteralKind::Float { base, empty_exponent } => {
            let mut kind = LitKind::Float;
            if empty_exponent || base != Base::Decimal {
                error = true;
                kind = LitKind::Err;
            }
            (kind, whole())
        }
    };
    (kind, sym, error)
}

/// The shared cooking as blaze-fmt now takes it: a fatal error is reported and the literal is
/// an error with all its text.
fn fmt_now(src: &str) -> (LitKind, String, bool) {
    let (cooked, errors) = cook(src).expect("a literal");
    match cooked {
        Ok((kind, sym)) => (kind, sym, !errors.is_empty()),
        Err(_) => {
            let (_, _, suffix_start) = literal(src).expect("a literal");
            (LitKind::Err, src[..suffix_start as usize].to_owned(), true)
        }
    }
}

fn agrees(src: &str) {
    let Some((kind, len, suffix_start)) = literal(src) else { return };
    let (cooked, errors) = cook(src).expect("a literal");
    let messages: Vec<String> = errors.iter().map(|e| e.message.to_string()).collect();
    for e in &errors {
        assert!(e.range.start <= e.range.end && e.range.end <= len, "{src:?}: {e:?} outside the token");
    }

    let mut old_errors = Vec::new();
    match (macro_eval::cook(src, suffix_start, kind, &mut old_errors), &cooked) {
        (Ok(old), Ok((kind, sym))) => {
            assert_eq!(old, (*kind, sym.clone()), "{src:?}");
            assert_eq!(old_errors, messages, "{src:?}");
        }
        (Err(macro_eval::Fatal), Err(fatal)) => {
            let (last, before) = old_errors.split_last().expect("a fatal error is pushed");
            assert_eq!(*last, fatal.message, "{src:?}");
            assert_eq!(before, &messages[..], "{src:?}");
        }
        (old, new) => panic!("{src:?}: macro-eval's copy {} where the shared one {new:?}", match old {
            Ok(_) => "cooked",
            Err(_) => "stopped",
        }),
    }

    let unterminated = matches!(
        kind,
        LiteralKind::Char { terminated: false }
            | LiteralKind::Byte { terminated: false }
            | LiteralKind::Str { terminated: false }
            | LiteralKind::ByteStr { terminated: false }
            | LiteralKind::CStr { terminated: false }
    );
    // An unterminated literal fails blaze-fmt's whole file, so its token was never seen.
    if !unterminated {
        assert_eq!(fmt_reference(src, suffix_start, kind), fmt_now(src), "{src:?}");
    }
}

const CASES: &[&str] = &[
    "'a'", "''", "'\\n'", "'\\x7f'", "'\\x80'", "'\\u{10ffff}'", "'\\u{110000}'", "'\\u{d800}'",
    "'\\u{}'", "'\\u{_1}'", "'\\u{1234567}'", "'\\u1'", "'\\u{1'", "'\\q'", "'\t'", "'\r'", "'\n'",
    "b'a'", "b'é'", "b'\\u{1}'", "b'\\q'", "b'\\xff'", "\"abc\"", "\"a\\\nb\"", "\"a\\\n\n b\"",
    "\"\\\r\"", "\"\r\"", "\"\\z\"", "\"abc", "b\"\\xff\"", "b\"é\"", "b\"abc", "c\"\\0\"",
    "c\"a\\u{0}\"", "c\"ok\"", "c\"x", "r\"a\rb\"", "r#\"x\"#", "r##\"x\"#", "r#~\"x\"#", "br\"é\"",
    "br#\"ok\"#suffix", "cr\"\0\"", "cr##\"y\"##", "\"x\"_", "\"x\"suf", "0b102", "0o78_9", "0b",
    "0x", "0xff", "1e", "1e+", "1.0e", "0b1.0", "0x1.0", "1.5f32", "0o7e1", "0b1e1", "12_u8",
    "'\\'", "\"\\\"", "b'\\'", "'\\u{d7ff}'",
];

#[test]
fn agrees_with_both_old_copies() {
    for src in CASES {
        assert!(literal(src).is_some(), "{src:?} lexes as a literal");
        agrees(src);
    }
}

#[test]
fn ranges_are_rustcs() {
    let (cooked, errors) = cook("0b1201").unwrap();
    assert_eq!(cooked.unwrap(), (LitKind::Err, "0b1201".to_owned()));
    assert_eq!(errors.iter().map(|e| e.range.clone()).collect::<Vec<_>>(), [3..4]);
    let (_, errors) = cook("\"a\\qb\"").unwrap();
    assert_eq!(errors, [CookError::new(2..4, "unknown character escape: `q`")]);
    let (cooked, _) = cook("r#~\"x\"#").unwrap();
    assert_eq!(
        cooked.unwrap_err().message,
        "found invalid character; only `#` is allowed in raw string delimitation: ~"
    );
}

#[test]
fn bare_crs_in_doc_comments() {
    let mut errors = Vec::new();
    doc_comment_errors(" a\rb\r", false, |e| errors.push(e));
    assert_eq!(errors, [
        CookError::new(2..3, "bare CR not allowed in doc-comment"),
        CookError::new(4..5, "bare CR not allowed in doc-comment"),
    ]);
    errors.clear();
    doc_comment_errors(" ok ", true, |e| errors.push(e));
    assert!(errors.is_empty());
}

#[hegel::test]
fn agrees_on_any_literal(tc: TestCase) {
    let src = tc.draw(gs::from_regex(
        "(b|c|r|br|cr)?#{0,2}[\"'][a-z0-9\\\\\"'#{}_\r\n\téxuU]{0,10}[\"'#]{0,3}[a-z_]{0,2}",
    ));
    agrees(&src);
}

#[hegel::test]
fn agrees_on_any_number(tc: TestCase) {
    let src = tc.draw(gs::from_regex("0[box]?[0-9a-f_.]{0,6}(e[+-]?[0-9]{0,2})?[a-z_0-9]{0,3}"));
    agrees(&src);
}

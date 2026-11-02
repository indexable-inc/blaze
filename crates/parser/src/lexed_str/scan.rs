//! The lexer's stage 2: tokens from the byte classes ([`super::classify`]).
//!
//! [`simple`] walks the start bits of a block in a register (simdjson's stage 2): every token
//! up to the block's first special start is an identifier, whitespace or one-byte punctuation,
//! known from its first byte, its length the distance to the next start. So the loop has no
//! test but its own and the keyword probe of an identifier; it packs into the token column a
//! block at a time and adds the block's checkpoints once.
//!
//! [`special`] lexes the token at a special start: numbers in every base, with fractions,
//! exponents and suffixes; line, block (nested) and doc comments, ended by the `\n` and the
//! `/`-or-`*` bitmaps; strings, byte strings, raw strings, raw byte strings, chars and byte
//! chars, ended by the quote bitmap, their escapes checked once with the converter's own
//! messages; lifetimes; raw identifiers; and the ASCII bytes `rustc_lexer` calls unknown. A
//! token it gives is the token `rustc_lexer` lexes there as the converter converts it, with no
//! error. It gives `None`, and the caller asks `rustc_lexer`, for a token with a lexer error, a
//! token an edition splits (a C string, a raw lifetime, `#"` and `##`, an unknown prefix), and
//! a token that starts with or runs into a byte of 0x80 or more outside a string or comment,
//! where only Unicode's tables can say where it ends. That is dispatch by token class, not a
//! second set of rules: the tests check both agree at every offset of every test file.

use blaze_phf::{name_hash_finish, name_hash_start, name_hash_word};
use rustc_literal_escaper::{Mode, unescape_byte, unescape_byte_str, unescape_char, unescape_str};

use super::{
    CHECKPOINT, KIND_BITS, LONG, LexedStr, can_be_raw, err_to_msg,
    classify::{Classes, NEWLINE, QUOTE, SIMPLE_PUNCT, SLASH_STAR, SPECIAL, START, is_simple_first},
};
use crate::{Edition, SyntaxKind, T, syntax_kind::keywords};

/// A simple token's kind by its first byte: `IDENT` for an identifier run (the keyword probe
/// decides), `WHITESPACE`, or the punctuation's kind; `EOF` for every special first byte.
const FIRST: [SyntaxKind; 256] = {
    let mut table = [SyntaxKind::EOF; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        if c.is_ascii_alphabetic() || c == b'_' {
            table[b] = SyntaxKind::IDENT;
        } else if matches!(c, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') {
            table[b] = SyntaxKind::WHITESPACE;
        }
        b += 1;
    }
    let punct: [SyntaxKind; 25] = [
        T![;],
        T![,],
        T![.],
        T!['('],
        T![')'],
        T!['{'],
        T!['}'],
        T!['['],
        T![']'],
        T![@],
        T![~],
        T![?],
        T![:],
        T![$],
        T![=],
        T![!],
        T![<],
        T![>],
        T![-],
        T![&],
        T![|],
        T![+],
        T![*],
        T![^],
        T![%],
    ];
    let mut i = 0;
    while i < punct.len() {
        table[SIMPLE_PUNCT[i] as usize] = punct[i];
        i += 1;
    }
    table
};

/// [`FLAGS`] of a simple first byte: an identifier run or a whitespace run.
const IDENT_RUN: u8 = 1;
const SPACE_RUN: u8 = 2;

const FLAGS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        table[b] = match FIRST[b] {
            SyntaxKind::IDENT => IDENT_RUN,
            SyntaxKind::WHITESPACE => SPACE_RUN,
            _ => 0,
        };
        b += 1;
    }
    table
};

/// The runs a byte right after them leaves to [`special`]: a quote or `#` after an identifier
/// makes it a prefix (`r#`, `b"`, `br"`, `c"`, an unknown one), and a byte of 0x80 or more may
/// continue either (an identifier, or U+0085, U+200E, U+200F, U+2028 or U+2029). Each of these
/// bytes is a special start, so only the run before a block's first special start is tested.
const STOPS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        if b >= 0x80 {
            table[b] = IDENT_RUN | SPACE_RUN;
        } else if matches!(b as u8, b'#' | b'"' | b'\'') {
            table[b] = IDENT_RUN;
        }
        b += 1;
    }
    table
};

/// [`LexedStr`]'s hash column, there only for [`LexedStr::new_hashed`].
type Names = Option<blaze_mem::scratch::Lent<Vec<u64>>>;

/// The kind of the simple token from `p` of `len` bytes. An identifier run is `_`, a keyword of
/// `edition` or `IDENT`, from one read of its bytes: an 8-byte window when it is 8 bytes or
/// fewer, which with `HASH` is also its [`blaze_phf::name_hash`], pushed onto `names`.
#[inline(always)]
fn kind_of<const HASH: bool>(text: &str, p: usize, len: usize, edition: Edition, names: &mut Names) -> SyntaxKind {
    let bytes = text.as_bytes();
    match FIRST[bytes[p] as usize] {
        SyntaxKind::IDENT if len <= 8 => {
            let window = keywords::ident_window(bytes, p, len);
            if HASH && let Some(names) = names.as_deref_mut() {
                names.push(name_hash_finish(name_hash_word(name_hash_start(len), window)));
            }
            match len {
                1 if window == b'_' as u64 => SyntaxKind::UNDERSCORE,
                _ => keywords::strict_by_window(window, len, edition).unwrap_or(SyntaxKind::IDENT),
            }
        }
        // Longer than every keyword.
        SyntaxKind::IDENT => {
            if HASH && let Some(names) = names.as_deref_mut() {
                names.push(blaze_phf::name_hash(&bytes[p..p + len]));
            }
            SyntaxKind::IDENT
        }
        kind => kind,
    }
}

/// The packed token from `p` to `e` (a simple token), and for one of [`LONG`] bytes or more its
/// length in `long` as token `index`.
#[inline(always)]
fn pack<const HASH: bool>(
    text: &str,
    (p, e): (usize, usize),
    edition: Edition,
    index: usize,
    (long, names): (&mut Vec<(u32, u32)>, &mut Names),
) -> u32 {
    let len = e - p;
    let kind = kind_of::<HASH>(text, p, len, edition, names) as u16 as u32;
    if len < LONG as usize {
        kind | (len as u32) << KIND_BITS
    } else {
        long.push((index as u32, len as u32));
        kind | LONG << KIND_BITS
    }
}

/// [`pack`] of a token shorter than 64 bytes: one that starts and ends in the same block.
#[inline(always)]
fn pack_short<const HASH: bool>(text: &str, p: usize, e: usize, edition: Edition, names: &mut Names) -> u32 {
    let len = e - p;
    debug_assert!(len < 64);
    let kind = kind_of::<HASH>(text, p, len, edition, names) as u16 as u32;
    kind | (len as u32) << KIND_BITS
}

/// How a block's window of tokens closed in [`simple`].
enum Closed {
    /// At the block's end: go on at the next block.
    Block,
    /// For the caller: at the text's end, at a run the byte after it may continue ([`STOPS`]),
    /// or at a special token [`special`] leaves to `rustc_lexer`.
    Stop,
    /// At a special token that ends past the block: its kind and length.
    Past(SyntaxKind, usize),
}

/// Lexes the tokens from `p` (a simple first byte, below the text's length) into `res`: the
/// simple ones a block at a time, and each special one [`special`] decides in the same window
/// when it ends in its block, so a block of text pays the window once however many special
/// tokens it has. Gives where it stopped: the text's end, the start of a run the byte after it
/// may continue ([`STOPS`]), or a special start, for [`special`] or `rustc_lexer`.
pub(super) fn simple<const HASH: bool>(
    res: &mut LexedStr<'_>,
    classes: &Classes<'_>,
    edition: Edition,
    mut p: usize,
) -> usize {
    let text: &str = res.text;
    let bytes = text.as_bytes();
    let mut starts = [0u32; 64];
    let mut block = p / 64;
    // Two shifts, as `p % 64 + 1` may be 64.
    let mut bits = classes.blocks[block][START] & ((!0u64 << (p % 64)) << 1);
    loop {
        if bits == 0 {
            block += 1;
            bits = classes.blocks[block][START];
            continue;
        }
        let LexedStr { checkpoints, tokens, long, names, .. } = &mut *res;
        let at = block * 64;
        let base = tokens.len();
        // A window ends at most 64 tokens: each at a start bit of the block, its own, or (a
        // special one) in the block past the start bit it began at.
        tokens.resize(base + 64, 0);
        let window: &mut [u32; 64] = (&mut tokens[base..base + 64]).try_into().expect("64 tokens");
        let mut n = 0;
        let closed = 'window: loop {
            let hard = bits & classes.blocks[block][SPECIAL];
            let first_hard = hard & hard.wrapping_neg();
            // The starts before the first special one (all of them without one).
            let mut run = bits & first_hard.wrapping_sub(1);
            if run != 0 && p < at {
                // The token carried in from a block before, the one that may be long.
                let e = at + run.trailing_zeros() as usize;
                run &= run - 1;
                window[0] = pack::<HASH>(text, (p, e), edition, base, (&mut *long, &mut *names));
                starts[0] = p as u32;
                n = 1;
                p = e;
            }
            while run != 0 {
                let e = at + run.trailing_zeros() as usize;
                run &= run - 1;
                window[n & 63] = pack_short::<HASH>(text, p, e, edition, names);
                starts[n & 63] = p as u32;
                n += 1;
                p = e;
            }
            if first_hard == 0 {
                break Closed::Block;
            }
            let e = at + first_hard.trailing_zeros() as usize;
            let after = bytes.get(e).copied().unwrap_or(0);
            if STOPS[after as usize] & FLAGS[bytes[p] as usize] != 0 {
                break Closed::Stop;
            }
            window[n & 63] = pack::<HASH>(text, (p, e), edition, base + n, (&mut *long, &mut *names));
            starts[n & 63] = p as u32;
            n += 1;
            p = e;
            // The special tokens from `p` that end in the block, up to a simple first byte.
            loop {
                if p == bytes.len() {
                    break 'window Closed::Stop;
                }
                if is_simple_first(bytes[p]) {
                    break;
                }
                let Some((kind, len)) = special(text, p, classes) else { break 'window Closed::Stop };
                if p + len >= at + 64 {
                    break 'window Closed::Past(kind, len);
                }
                if let Some(names) = names.as_deref_mut()
                    && LexedStr::has_name_hash(kind)
                {
                    // A raw identifier, as `LexedStr::push` hashes it.
                    let name = &text[p..p + len];
                    names.push(blaze_phf::name_hash(name.strip_prefix("r#").unwrap_or(name).as_bytes()));
                }
                window[n & 63] = kind as u16 as u32 | (len as u32) << KIND_BITS;
                starts[n & 63] = p as u32;
                n += 1;
                p += len;
            }
            bits = classes.blocks[block][START] & ((!0u64 << (p % 64)) << 1);
        };
        tokens.truncate(base + n);
        let mut i = base.next_multiple_of(CHECKPOINT);
        while i < base + n {
            checkpoints.push(starts[(i - base) & 63]);
            i += CHECKPOINT;
        }
        match closed {
            Closed::Block => {
                block += 1;
                bits = classes.blocks[block][START];
            }
            Closed::Stop => return p,
            Closed::Past(kind, len) => {
                res.push(kind, p, len);
                p += len;
                if p == bytes.len() || !is_simple_first(bytes[p]) {
                    return p;
                }
                block = p / 64;
                bits = classes.blocks[block][START] & ((!0u64 << (p % 64)) << 1);
            }
        }
    }
}

/// The token `rustc_lexer` lexes at byte `at` of `text` (a whole text: no frontmatter, `at` a
/// token start below its length), as the converter converts it, when the bytes decide it and it
/// has no lexer error; `None` when `rustc_lexer` must lex it. The kind is final: a raw
/// identifier is `IDENT` whatever its name.
pub(super) fn special(text: &str, at: usize, classes: &Classes<'_>) -> Option<(SyntaxKind, usize)> {
    let bytes = text.as_bytes();
    let byte = |i: usize| bytes.get(i).copied();
    match bytes[at] {
        b'0'..=b'9' => number(bytes, at, classes),
        b'/' => match byte(at + 1) {
            Some(b'/') => {
                let kind = match byte(at + 2) {
                    Some(b'!') => SyntaxKind::INNER_DOC_COMMENT,
                    Some(b'/') if byte(at + 3) != Some(b'/') => SyntaxKind::OUTER_DOC_COMMENT,
                    _ => SyntaxKind::COMMENT,
                };
                Some((kind, classes.next_bit(NEWLINE, at + 2) - at))
            }
            Some(b'*') => block_comment(bytes, at, classes),
            _ => Some((T![/], 1)),
        },
        b'#' => match byte(at + 1) {
            Some(b'"' | b'#') => None,
            _ => Some((T![#], 1)),
        },
        b'"' => {
            let (close, escaped) = close_quote(bytes, at + 1, classes)?;
            let end = suffix_end(bytes, close + 1, classes)?;
            (!escaped || clean_str(&text[at + 1..close], Mode::Str))
                .then_some((SyntaxKind::STRING, end - at))
        }
        b'\'' => lifetime_or_char(text, at, classes),
        b'r' | b'b' => prefixed(text, at, classes),
        b'`' | b'\\' | 0x00..=0x08 | 0x0E..=0x1F | 0x7F => Some((SyntaxKind::ERROR, 1)),
        _ => None,
    }
}

/// `rustc_lexer`'s `number` and literal suffix at the digit at `at`: an integer or a float, or
/// `None` for an empty integer or exponent (errors) or a byte of 0x80 or more where an
/// identifier character could continue it.
fn number(bytes: &[u8], at: usize, classes: &Classes<'_>) -> Option<(SyntaxKind, usize)> {
    // `rustc_lexer` reads the end of the text as `\0`, which no test below matches either.
    let byte = |i: usize| bytes.get(i).copied().unwrap_or(0);
    let digits = |mut i: usize, hex: bool| -> (usize, bool) {
        let mut any = false;
        loop {
            match byte(i) {
                b'_' => {}
                b'0'..=b'9' => any = true,
                b'a'..=b'f' | b'A'..=b'F' if hex => any = true,
                _ => return (i, any),
            }
            i += 1;
        }
    };
    let exponent = |mut i: usize| -> Option<usize> {
        if matches!(byte(i), b'-' | b'+') {
            i += 1;
        }
        let (end, any) = digits(i, false);
        any.then_some(end)
    };
    let mut i = at + 1;
    if bytes[at] == b'0' {
        match byte(i) {
            base @ (b'b' | b'o' | b'x') => {
                let (end, any) = digits(i + 1, base == b'x');
                if !any {
                    return None;
                }
                i = end;
            }
            b'0'..=b'9' | b'_' => i = digits(i, false).0,
            b'.' | b'e' | b'E' => {}
            _ => return Some((SyntaxKind::INT_NUMBER, suffix_end(bytes, i, classes)? - at)),
        }
    } else {
        i = digits(i, false).0;
    }
    let kind = match byte(i) {
        b'.' => match byte(i + 1) {
            b if b >= 0x80 => return None,
            // `0..2` and `1.foo` take no fraction.
            b'.' | b'_' => SyntaxKind::INT_NUMBER,
            b if b.is_ascii_alphabetic() => SyntaxKind::INT_NUMBER,
            _ => {
                i += 1;
                if byte(i).is_ascii_digit() {
                    i = digits(i, false).0;
                    if matches!(byte(i), b'e' | b'E') {
                        i = exponent(i + 1)?;
                    }
                }
                SyntaxKind::FLOAT_NUMBER
            }
        },
        b'e' | b'E' => {
            i = exponent(i + 1)?;
            SyntaxKind::FLOAT_NUMBER
        }
        _ => SyntaxKind::INT_NUMBER,
    };
    Some((kind, suffix_end(bytes, i, classes)? - at))
}

/// The end of the literal suffix at `i` (`rustc_lexer`'s `eat_identifier`): `i` itself when
/// none starts there, or `None` when a byte of 0x80 or more could start or continue it.
#[inline]
fn suffix_end(bytes: &[u8], i: usize, classes: &Classes<'_>) -> Option<usize> {
    match bytes.get(i).copied() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {
            let end = classes.next_start(i);
            bytes.get(end).is_none_or(|&b| b < 0x80).then_some(end)
        }
        Some(b) if b >= 0x80 => None,
        _ => Some(i),
    }
}

/// `rustc_lexer`'s `block_comment` at the `/*` at `at`, its nesting counted from `/` and `*`
/// bit to bit; `None` when unterminated (an error).
fn block_comment(bytes: &[u8], at: usize, classes: &Classes<'_>) -> Option<(SyntaxKind, usize)> {
    let byte = |i: usize| bytes.get(i).copied();
    let kind = match byte(at + 2) {
        Some(b'!') => SyntaxKind::INNER_DOC_COMMENT,
        // `/***` and `/**/` are not doc comments.
        Some(b'*') if !matches!(byte(at + 3), Some(b'*' | b'/')) => SyntaxKind::OUTER_DOC_COMMENT,
        _ => SyntaxKind::COMMENT,
    };
    let (mut from, mut depth) = (at + 2, 1usize);
    loop {
        let found = classes.next_bit(SLASH_STAR, from);
        match (byte(found)?, byte(found + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                from = found + 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                from = found + 2;
                if depth == 0 {
                    return Some((kind, from - at));
                }
            }
            _ => from = found + 1,
        }
    }
}

/// The closing `"` of the string whose contents start at `from` (`rustc_lexer`'s
/// `double_quoted_string`), from quote bit to quote bit, a `\` taking the byte after it, and
/// whether there was a `\`; `None` when unterminated (an error).
#[inline]
fn close_quote(bytes: &[u8], mut from: usize, classes: &Classes<'_>) -> Option<(usize, bool)> {
    let mut escaped = false;
    loop {
        let found = classes.next_bit(QUOTE, from);
        match bytes.get(found) {
            Some(b'"') => return Some((found, escaped)),
            // A `\`: the byte after it is never the end (a byte, so a UTF-8 lead at most; the
            // rest of its character has no quote bit).
            Some(_) if found + 1 < bytes.len() => (from, escaped) = (found + 2, true),
            _ => return None,
        }
    }
}

/// The closing `'` of the char whose contents start at `i` (`rustc_lexer`'s
/// `single_quoted_string` past the one-character case, a byte at a time: a UTF-8 continuation
/// byte matches no arm); `None` when unterminated (an error).
fn close_char(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        match bytes.get(i).copied() {
            Some(b'\'') => return Some(i),
            Some(b'/') | None => return None,
            Some(b'\n') if bytes.get(i + 1) != Some(&b'\'') => return None,
            Some(b'\\') => i += 2,
            Some(_) => i += 1,
        }
    }
}

/// The closing `'` of a char or byte whose contents start at `i`: right after one character
/// that is not `\`, else [`close_char`].
#[inline]
fn close_single(bytes: &[u8], i: usize) -> Option<usize> {
    let first = *bytes.get(i)?;
    // `i` is a character boundary, so `first` is ASCII or a UTF-8 lead.
    let len = if first < 0x80 { 1 } else { first.leading_ones() as usize };
    if first != b'\\' && bytes.get(i + len) == Some(&b'\'') {
        return Some(i + len);
    }
    close_char(bytes, i)
}

/// Whether the converter gives no error for these string contents: every escape error it
/// would report has no message.
fn clean_str(text: &str, mode: Mode) -> bool {
    let mut clean = true;
    let mut check = |error: Option<rustc_literal_escaper::EscapeError>| {
        clean &= error.is_none_or(|e| err_to_msg(e, mode).is_empty());
    };
    match mode {
        Mode::ByteStr => unescape_byte_str(text, |_, it| check(it.err())),
        _ => unescape_str(text, |_, it| check(it.err())),
    }
    clean
}

/// `rustc_lexer`'s `lifetime_or_char` at the `'` at `at`.
fn lifetime_or_char(text: &str, at: usize, classes: &Classes<'_>) -> Option<(SyntaxKind, usize)> {
    let bytes = text.as_bytes();
    // A lone `'` at the end is an unterminated char.
    let first = *bytes.get(at + 1)?;
    let first_len = if first < 0x80 { 1 } else { first.leading_ones() as usize };
    if bytes.get(at + 1 + first_len) != Some(&b'\'') {
        if first >= 0x80 {
            // Whether it is a lifetime is Unicode's `XID_Start`.
            return None;
        }
        if first.is_ascii_digit() {
            // A lifetime that starts with a number, or a char of more than one: errors.
            return None;
        }
        if first.is_ascii_alphabetic() || first == b'_' {
            // `'r#` is a raw lifetime or an unknown prefix, which editions split.
            if first == b'r' && bytes.get(at + 2) == Some(&b'#') {
                return None;
            }
            let end = classes.next_start(at + 1);
            return match bytes.get(end).copied() {
                // A char of more than one character, an unknown prefix, or a longer name.
                Some(b'\'' | b'#') => None,
                Some(b) if b >= 0x80 => None,
                _ => Some((SyntaxKind::LIFETIME_IDENT, end - at)),
            };
        }
    }
    let close = close_single(bytes, at + 1)?;
    let end = suffix_end(bytes, close + 1, classes)?;
    unescape_char(&text[at + 1..close])
        .err()
        .is_none_or(|e| err_to_msg(e, Mode::Char).is_empty())
        .then_some((SyntaxKind::CHAR, end - at))
}

/// `rustc_lexer`'s `r` and `b` prefixes at the identifier run at `at`: a raw identifier, a raw
/// string, a raw byte string, a byte string or a byte. Every other prefix (`c`, `cr` and the
/// unknown ones) is the editions' to split, and not a prefix at all is not a token here.
fn prefixed(text: &str, at: usize, classes: &Classes<'_>) -> Option<(SyntaxKind, usize)> {
    let bytes = text.as_bytes();
    let run = classes.next_start(at);
    match (&bytes[at..run], bytes.get(run).copied()) {
        (b"r", Some(b'#')) => match bytes.get(run + 1).copied() {
            Some(b) if b.is_ascii_alphabetic() || b == b'_' => {
                let end = classes.next_start(run + 1);
                let name = &text[run + 1..end];
                (bytes.get(end).is_none_or(|&b| b < 0x80) && can_be_raw(name))
                    .then_some((SyntaxKind::IDENT, end - at))
            }
            // Whether it is a raw identifier is Unicode's `XID_Start`.
            Some(b) if b >= 0x80 => None,
            _ => raw_string(bytes, at, run, SyntaxKind::STRING, classes),
        },
        (b"r", Some(b'"')) => raw_string(bytes, at, run, SyntaxKind::STRING, classes),
        (b"br", Some(b'#' | b'"')) => raw_string(bytes, at, run, SyntaxKind::BYTE_STRING, classes),
        (b"b", Some(b'"')) => {
            let (close, _) = close_quote(bytes, run + 1, classes)?;
            let end = suffix_end(bytes, close + 1, classes)?;
            clean_str(&text[run + 1..close], Mode::ByteStr).then_some((SyntaxKind::BYTE_STRING, end - at))
        }
        (b"b", Some(b'\'')) => {
            let close = close_single(bytes, run + 1)?;
            let end = suffix_end(bytes, close + 1, classes)?;
            unescape_byte(&text[run + 1..close])
                .err()
                .is_none_or(|e| err_to_msg(e, Mode::Byte).is_empty())
                .then_some((SyntaxKind::BYTE, end - at))
        }
        _ => None,
    }
}

/// `rustc_lexer`'s `raw_double_quoted_string` and suffix, the token starting at `at` and its
/// `#`s at `hashes`: the end is the first `"` with as many `#`s after it, found from quote bit
/// to quote bit. `None` for more than 255 `#`s, no `"` after them, or no end (errors).
fn raw_string(
    bytes: &[u8],
    at: usize,
    hashes: usize,
    kind: SyntaxKind,
    classes: &Classes<'_>,
) -> Option<(SyntaxKind, usize)> {
    let n = bytes[hashes..].iter().take_while(|&&b| b == b'#').count();
    let open = hashes + n;
    if n > 255 || bytes.get(open) != Some(&b'"') {
        return None;
    }
    let mut from = open + 1;
    let close = loop {
        let found = classes.next_bit(QUOTE, from);
        match bytes.get(found) {
            None => return None,
            Some(b'"') if bytes[found + 1..].iter().take(n).take_while(|&&b| b == b'#').count() == n => {
                break found + 1 + n;
            }
            // A `"` with too few `#`s, or a `\`, which a raw string does not escape with.
            Some(_) => from = found + 1,
        }
    };
    Some((kind, suffix_end(bytes, close, classes)? - at))
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::super::{
        Converter, LexedStr,
        classify::{Classes, is_simple_first},
        differential,
    };
    use super::special;
    use crate::Edition;

    /// Wherever [`special`] gives a token, it is the token `rustc_lexer` lexes there as the
    /// converter converts it, with no error: at every character boundary of the parser's
    /// sources, its test files and random texts.
    #[test]
    fn special_lexes_as_rustc_lexer() {
        let mut rng = SplitMix64::new(11);
        let mut texts = differential::parser_files();
        texts.extend((0..3000).map(|_| differential::random_text(&mut rng)));
        for text in &texts {
            let mut words = Vec::new();
            let classes = Classes::new(text.as_bytes(), &mut words);
            for at in (0..text.len()).filter(|&at| text.is_char_boundary(at)) {
                let Some((kind, len)) = special(text, at, &classes) else { continue };
                let rest = &text[at..];
                let reference = rustc_lexer::tokenize(rest, rustc_lexer::FrontmatterAllowed::No)
                    .next()
                    .expect("a token where text is left");
                let mut conv = Converter::new(Edition::CURRENT, rest);
                conv.extend_token(&reference.kind, &rest[..reference.len as usize]);
                assert!(
                    (kind, len) == (conv.res.raw_kind(0), conv.offset)
                        && conv.res.tokens.len() == 1
                        && conv.res.error.is_empty(),
                    "at {at} of {:?}: {:?}",
                    &text[at..text.len().min(at + 40)],
                    (kind, len, reference.kind),
                );
            }
        }
    }

    /// In real Rust, the parser's own sources, `special` decides at least 99 in 100 of the
    /// tokens at a special first byte: `rustc_lexer` lexes only what has a lexer error, what an
    /// edition splits, or what runs into a byte of 0x80 or more.
    #[test]
    fn special_decides_nearly_every_special_token() {
        let (mut asked, mut decided) = (0usize, 0usize);
        for text in differential::parser_sources() {
            let lexed = LexedStr::lex::<false>(Edition::CURRENT, &text);
            let mut words = Vec::new();
            let classes = Classes::new(text.as_bytes(), &mut words);
            for i in 0..lexed.len() {
                let at = lexed.text_start(i);
                if is_simple_first(text.as_bytes()[at]) {
                    continue;
                }
                asked += 1;
                decided += usize::from(special(&text, at, &classes).is_some());
            }
        }
        assert!(asked > 5_000, "{asked} special tokens in the parser's sources");
        assert!(decided * 100 >= asked * 99, "special decides {decided} of {asked}");
    }
}

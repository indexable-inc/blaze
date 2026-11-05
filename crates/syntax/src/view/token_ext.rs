//! The hand-written accessors of token views, `ast/token_ext.rs` on views: comments, strings
//! and numbers. Text is lent from the tree; a literal's value is built only when unescaping
//! changes it.

use std::{borrow::Cow, num::ParseIntError, ops::Range};

use rustc_literal_escaper::{
    EscapeError, MixedUnit, unescape_byte, unescape_byte_str, unescape_c_str, unescape_char,
    unescape_str,
};
use stdx::always;

use crate::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstToken, Token},
};

pub use crate::ast::{CommentKind, CommentShape, QuoteOffsets, Radix};

use crate::ast::AttrKind;

impl<'t> ast::Comment<'t> {
    pub fn shape(self) -> CommentShape {
        CommentShape::from_text(self.text())
    }

    /// Returns the text without the `//` or `/*...*/` markers.
    pub fn text_without_markers(self) -> &'t str {
        let text = self.text();
        let shape = CommentShape::from_text(text);
        let text = &text[2..];
        match shape {
            CommentShape::Block => text.strip_suffix("*/").unwrap_or(text),
            CommentShape::Line => text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnyComment<'t> {
    syntax: Token<'t>,
}

impl<'t> AstToken<'t> for AnyComment<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(
            kind,
            SyntaxKind::COMMENT | SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT
        )
    }

    fn cast(syntax: Token<'t>) -> Option<Self> {
        if Self::can_cast(syntax.kind()) { Some(Self { syntax }) } else { None }
    }

    fn syntax(&self) -> Token<'t> {
        self.syntax
    }
}

impl<'t> AnyComment<'t> {
    pub fn shape(self) -> CommentShape {
        CommentShape::from_text(self.text_with_markers())
    }

    /// The kind of doc comment this is; `None` for a plain `COMMENT`, the only other kind a cast
    /// lets in.
    #[inline]
    pub fn doc_kind(self) -> Option<AttrKind> {
        match self.syntax.kind() {
            SyntaxKind::COMMENT => None,
            SyntaxKind::INNER_DOC_COMMENT => Some(AttrKind::Inner),
            SyntaxKind::OUTER_DOC_COMMENT => Some(AttrKind::Outer),
            kind => panic!("{kind:?} at {:?} is not a comment", self.syntax.text_range()),
        }
    }

    pub fn kind(self) -> CommentKind {
        CommentKind { shape: self.shape(), doc: self.doc_kind() }
    }

    pub fn prefix(self) -> &'static str {
        self.kind().prefix()
    }

    pub fn is_inner(self) -> bool {
        self.doc_kind() == Some(AttrKind::Inner)
    }

    pub fn is_outer(self) -> bool {
        self.doc_kind() == Some(AttrKind::Outer)
    }

    /// Returns the text with the `/*...*/` or `//...` or `/**...*/` or `/*!...*/` or `///...` or `//!...` markers.
    #[inline]
    pub fn text_with_markers(self) -> &'t str {
        self.syntax.text()
    }

    /// Returns the textual content of a doc comment node as a single string with prefix and suffix removed.
    pub fn text(self) -> &'t str {
        let shape = self.shape();
        let prefix_len = if self.doc_kind().is_some() { 3 } else { 2 };
        let text = &self.text_with_markers()[prefix_len..];
        if shape == CommentShape::Block {
            // The `*/` may not exist because of recovery.
            text.strip_suffix("*/").unwrap_or(text)
        } else {
            text
        }
    }
}

impl<'t> ast::Whitespace<'t> {
    pub fn spans_multiple_lines(self) -> bool {
        let text = self.text();
        text.find('\n').is_some_and(|idx| text[idx + 1..].contains('\n'))
    }
}

/// The quotes and contents of `literal`, relative to its start: `QuoteOffsets::new`.
fn quote_offsets_of(literal: &str) -> Option<QuoteOffsets> {
    let left_quote = literal.find('"')?;
    let right_quote = literal.rfind('"')?;
    if left_quote == right_quote {
        // `literal` only contains one quote
        return None;
    }

    let start = TextSize::from(0);
    let left_quote = TextSize::try_from(left_quote).ok()? + TextSize::of('"');
    let right_quote = TextSize::try_from(right_quote).ok()?;
    let end = TextSize::of(literal);

    Some(QuoteOffsets {
        quotes: (TextRange::new(start, left_quote), TextRange::new(right_quote, end)),
        contents: TextRange::new(left_quote, right_quote),
    })
}

pub trait IsString<'t>: AstToken<'t> {
    fn raw_prefix(&self) -> &'static str;
    fn unescape(&self, s: &str, callback: impl FnMut(Range<usize>, Result<char, EscapeError>));
    fn is_raw(&self) -> bool {
        self.text().starts_with(self.raw_prefix())
    }
    fn quote_offsets(&self) -> Option<QuoteOffsets> {
        let offsets = quote_offsets_of(self.text())?;
        let o = self.syntax().text_range().start();
        Some(QuoteOffsets {
            quotes: (offsets.quotes.0 + o, offsets.quotes.1 + o),
            contents: offsets.contents + o,
        })
    }
    fn text_range_between_quotes(&self) -> Option<TextRange> {
        self.quote_offsets().map(|it| it.contents)
    }
    fn text_without_quotes(&self) -> &'t str {
        let text = self.text();
        let Some(offsets) = self.text_range_between_quotes() else { return text };
        &text[offsets - self.syntax().text_range().start()]
    }
    fn open_quote_text_range(&self) -> Option<TextRange> {
        self.quote_offsets().map(|it| it.quotes.0)
    }
    fn close_quote_text_range(&self) -> Option<TextRange> {
        self.quote_offsets().map(|it| it.quotes.1)
    }
    fn escaped_char_ranges(&self, mut cb: impl FnMut(TextRange, Result<char, EscapeError>)) {
        let Some(text_range_no_quotes) = self.text_range_between_quotes() else { return };

        let start = self.syntax().text_range().start();
        let text = &self.text()[text_range_no_quotes - start];
        let offset = text_range_no_quotes.start() - start;

        if self.is_raw() {
            let mut pos = offset;
            for c in text.chars() {
                let len = TextSize::of(c);
                cb(TextRange::at(pos, len), Ok(c));
                pos += len;
            }
            return;
        }

        self.unescape(text, |range: Range<usize>, unescaped_char| {
            if let Some((s, e)) = range.start.try_into().ok().zip(range.end.try_into().ok()) {
                cb(TextRange::new(s, e) + offset, unescaped_char);
            }
        });
    }
    fn map_range_up(&self, range: TextRange) -> Option<TextRange> {
        let contents_range = self.text_range_between_quotes()?;
        if always!(TextRange::up_to(contents_range.len()).contains_range(range)) {
            Some(range + contents_range.start())
        } else {
            None
        }
    }
    fn map_offset_down(&self, offset: TextSize) -> Option<TextSize> {
        let contents_range = self.text_range_between_quotes()?;
        offset.checked_sub(contents_range.start())
    }
}

impl<'t> IsString<'t> for ast::String<'t> {
    fn raw_prefix(&self) -> &'static str {
        "r"
    }
    fn unescape(&self, s: &str, cb: impl FnMut(Range<usize>, Result<char, EscapeError>)) {
        unescape_str(s, cb)
    }
}

impl<'t> ast::String<'t> {
    pub fn value(self) -> Result<Cow<'t, str>, EscapeError> {
        let text = self.text();
        let text_range = self.text_range_between_quotes().ok_or(EscapeError::LoneSlash)?;
        let text = &text[text_range - self.syntax.text_range().start()];
        if text.contains("\r\n") {
            // alloc: a value with CRLF line endings is rebuilt with `\n`, as the handle does.
            let text = text.replace("\r\n", "\n");
            return unescaped_str(&text, self.is_raw()).map(|it| Cow::Owned(it.into_owned()));
        }
        unescaped_str(text, self.is_raw())
    }
}

/// The value of the literal with `text` between its quotes. The text has the line endings
/// of a source file the compiler has read, which are `\n` only.
fn unescaped_str(text: &str, raw: bool) -> Result<Cow<'_, str>, EscapeError> {
    if raw {
        return Ok(Cow::Borrowed(text));
    }

    // alloc: grown only once an escape makes the value differ from the text.
    let mut buf = std::string::String::new();
    let mut prev_end = 0;
    let mut has_error = None;
    unescape_str(text, |char_range, unescaped_char| match (unescaped_char, buf.capacity() == 0) {
        (Ok(c), false) => buf.push(c),
        (Ok(_), true) if char_range.len() == 1 && char_range.start == prev_end => {
            prev_end = char_range.end
        }
        (Ok(c), true) => {
            buf.reserve_exact(text.len());
            buf.push_str(&text[..prev_end]);
            buf.push(c);
        }
        (Err(e), _) if e.is_fatal() => has_error = Some(e),
        // A warning about a line continuation, which gives no character.
        (Err(_), _) => {}
    });

    match (has_error, buf.capacity() == 0) {
        (Some(e), _) => Err(e),
        (None, true) => Ok(Cow::Borrowed(&text[..prev_end])),
        (None, false) => Ok(Cow::Owned(buf)),
    }
}

impl<'t> IsString<'t> for ast::ByteString<'t> {
    fn raw_prefix(&self) -> &'static str {
        "br"
    }
    fn unescape(&self, s: &str, mut callback: impl FnMut(Range<usize>, Result<char, EscapeError>)) {
        unescape_byte_str(s, |range, res| callback(range, res.map(char::from)))
    }
}

impl<'t> ast::ByteString<'t> {
    pub fn value(self) -> Result<Cow<'t, [u8]>, EscapeError> {
        let text = self.text();
        let text_range = self.text_range_between_quotes().ok_or(EscapeError::LoneSlash)?;
        let text = &text[text_range - self.syntax.text_range().start()];
        if text.contains("\r\n") {
            // alloc: a value with CRLF line endings is rebuilt with `\n`, as the handle does.
            let text = text.replace("\r\n", "\n");
            return unescaped_byte_str(&text, self.is_raw()).map(|it| Cow::Owned(it.into_owned()));
        }
        unescaped_byte_str(text, self.is_raw())
    }
}

/// The value of the literal with `text` between its quotes. The text has the line endings
/// of a source file the compiler has read, which are `\n` only.
fn unescaped_byte_str(text: &str, raw: bool) -> Result<Cow<'_, [u8]>, EscapeError> {
    if raw {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }

    // alloc: grown only once an escape makes the value differ from the text.
    let mut buf: Vec<u8> = Vec::new();
    let mut prev_end = 0;
    let mut has_error = None;
    unescape_byte_str(text, |char_range, unescaped_byte| {
        match (unescaped_byte, buf.capacity() == 0) {
            (Ok(b), false) => buf.push(b),
            (Ok(_), true) if char_range.len() == 1 && char_range.start == prev_end => {
                prev_end = char_range.end
            }
            (Ok(b), true) => {
                buf.reserve_exact(text.len());
                buf.extend_from_slice(&text.as_bytes()[..prev_end]);
                buf.push(b);
            }
            (Err(e), _) if e.is_fatal() => has_error = Some(e),
            // A warning about a line continuation, which gives no character.
            (Err(_), _) => {}
        }
    });

    match (has_error, buf.capacity() == 0) {
        (Some(e), _) => Err(e),
        (None, true) => Ok(Cow::Borrowed(&text.as_bytes()[..prev_end])),
        (None, false) => Ok(Cow::Owned(buf)),
    }
}

impl<'t> IsString<'t> for ast::CString<'t> {
    fn raw_prefix(&self) -> &'static str {
        "cr"
    }
    // NOTE: This method should only be used for highlighting ranges. The unescaped
    // char/byte is not used. For simplicity, we return an arbitrary placeholder char.
    fn unescape(&self, s: &str, mut callback: impl FnMut(Range<usize>, Result<char, EscapeError>)) {
        unescape_c_str(s, |range, _res| callback(range, Ok('_')))
    }
}

impl<'t> ast::CString<'t> {
    pub fn value(self) -> Result<Cow<'t, [u8]>, EscapeError> {
        let text = self.text();
        let text_range = self.text_range_between_quotes().ok_or(EscapeError::LoneSlash)?;
        let text = &text[text_range - self.syntax.text_range().start()];
        if text.contains("\r\n") {
            // alloc: a value with CRLF line endings is rebuilt with `\n`, as the handle does.
            let text = text.replace("\r\n", "\n");
            return unescaped_c_str(&text, self.is_raw()).map(|it| Cow::Owned(it.into_owned()));
        }
        unescaped_c_str(text, self.is_raw())
    }
}

/// The value of the literal with `text` between its quotes. The text has the line endings
/// of a source file the compiler has read, which are `\n` only.
fn unescaped_c_str(text: &str, raw: bool) -> Result<Cow<'_, [u8]>, EscapeError> {
    if raw {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }

    // alloc: grown only once an escape makes the value differ from the text.
    let mut buf = Vec::new();
    let mut prev_end = 0;
    let mut has_error = None;
    let extend_unit = |buf: &mut Vec<u8>, unit: MixedUnit| match unit {
        MixedUnit::Char(c) => buf.extend(c.get().encode_utf8(&mut [0; 4]).as_bytes()),
        MixedUnit::HighByte(b) => buf.push(b.get()),
    };
    unescape_c_str(text, |char_range, unescaped| match (unescaped, buf.capacity() == 0) {
        (Ok(u), false) => extend_unit(&mut buf, u),
        (Ok(_), true) if char_range.len() == 1 && char_range.start == prev_end => {
            prev_end = char_range.end
        }
        (Ok(u), true) => {
            buf.reserve_exact(text.len());
            buf.extend(&text.as_bytes()[..prev_end]);
            extend_unit(&mut buf, u);
        }
        (Err(e), _) if e.is_fatal() => has_error = Some(e),
        // A warning about a line continuation, which gives no character.
        (Err(_), _) => {}
    });

    match (has_error, buf.capacity() == 0) {
        (Some(e), _) => Err(e),
        (None, true) => Ok(Cow::Borrowed(&text.as_bytes()[..prev_end])),
        (None, false) => Ok(Cow::Owned(buf)),
    }
}

impl<'t> ast::IntNumber<'t> {
    pub fn radix(self) -> Radix {
        match self.text().get(..2).unwrap_or_default() {
            "0b" => Radix::Binary,
            "0o" => Radix::Octal,
            "0x" => Radix::Hexadecimal,
            _ => Radix::Decimal,
        }
    }

    pub fn split_into_parts(self) -> (&'t str, &'t str, &'t str) {
        let radix = self.radix();
        let prefix_len = match radix {
            Radix::Decimal => 0,
            _ => 2,
        };
        let (prefix, mut text) = self.text().split_at(prefix_len);

        let is_suffix_start: fn(&(usize, char)) -> bool = match radix {
            Radix::Hexadecimal => |(_, c)| matches!(c, 'g'..='z' | 'G'..='Z'),
            _ => |(_, c)| c.is_ascii_alphabetic(),
        };

        let mut suffix = "";
        if let Some((suffix_start, _)) = text.char_indices().find(is_suffix_start) {
            let (text2, suffix2) = text.split_at(suffix_start);
            text = text2;
            suffix = suffix2;
        };

        (prefix, text, suffix)
    }

    /// The value of the digits, `_`s skipped: `u128::from_str_radix` of them with the `_`s
    /// removed, the same value and the same error, without building the string.
    pub fn value(self) -> Result<u128, ParseIntError> {
        let (_, text, _) = self.split_into_parts();
        digits_value(text, self.radix() as u32)
    }

    pub fn suffix(self) -> Option<&'t str> {
        let (_, _, suffix) = self.split_into_parts();
        if suffix.is_empty() { None } else { Some(suffix) }
    }

    pub fn value_string(self) -> std::string::String {
        let (_, text, _) = self.split_into_parts();
        // alloc: the digits without `_`, a new string by its meaning, as the handle builds it.
        text.replace('_', "")
    }
}

/// `u128::from_str_radix(&text.replace('_', ""), radix)`, read in place: an optional leading
/// `+`, then digits of `radix`, the first bad digit or overflow failing as `from_str_radix`
/// fails on it.
pub(super) fn digits_value(text: &str, radix: u32) -> Result<u128, ParseIntError> {
    let mut digits = text.bytes().filter(|&b| b != b'_').peekable();
    let signed = digits.next_if_eq(&b'+').is_some();
    if digits.peek().is_none() {
        return Err(if signed { invalid_digit() } else { empty() });
    }
    let mut value: u128 = 0;
    for byte in digits {
        let digit = char::from(byte).to_digit(radix).ok_or_else(invalid_digit)?;
        value = value
            .checked_mul(u128::from(radix))
            .and_then(|it| it.checked_add(u128::from(digit)))
            .ok_or_else(pos_overflow)?;
    }
    Ok(value)
}

#[cold]
fn empty() -> ParseIntError {
    "".parse::<u128>().expect_err("an empty string is no number")
}

#[cold]
fn invalid_digit() -> ParseIntError {
    "z".parse::<u128>().expect_err("`z` is no decimal digit")
}

#[cold]
fn pos_overflow() -> ParseIntError {
    "340282366920938463463374607431768211456".parse::<u128>().expect_err("2^128 overflows u128")
}

impl<'t> ast::FloatNumber<'t> {
    pub fn split_into_parts(self) -> (&'t str, &'t str) {
        let text = self.text();
        let mut float_text = text;
        let mut suffix = "";
        let mut indices = text.char_indices();
        if let Some((mut suffix_start, c)) = indices.by_ref().find(|(_, c)| c.is_ascii_alphabetic())
        {
            if c == 'e' || c == 'E' {
                if let Some(suffix_start_tuple) = indices.find(|(_, c)| c.is_ascii_alphabetic()) {
                    suffix_start = suffix_start_tuple.0;

                    float_text = &text[..suffix_start];
                    suffix = &text[suffix_start..];
                }
            } else {
                float_text = &text[..suffix_start];
                suffix = &text[suffix_start..];
            }
        }

        (float_text, suffix)
    }

    pub fn suffix(self) -> Option<&'t str> {
        let (_, suffix) = self.split_into_parts();
        if suffix.is_empty() { None } else { Some(suffix) }
    }

    pub fn value_string(self) -> std::string::String {
        let (text, _) = self.split_into_parts();
        // alloc: the digits without `_`, a new string by its meaning, as the handle builds it.
        text.replace('_', "")
    }
}

impl<'t> ast::Char<'t> {
    pub fn value(self) -> Result<char, EscapeError> {
        let mut text = self.text();
        if text.starts_with('\'') {
            text = &text[1..];
        } else {
            return Err(EscapeError::ZeroChars);
        }
        if text.ends_with('\'') {
            text = &text[0..text.len() - 1];
        }

        unescape_char(text)
    }
}

impl<'t> ast::Byte<'t> {
    pub fn value(self) -> Result<u8, EscapeError> {
        let mut text = self.text();
        if text.starts_with("b\'") {
            text = &text[2..];
        } else {
            return Err(EscapeError::ZeroChars);
        }
        if text.ends_with('\'') {
            text = &text[0..text.len() - 1];
        }

        unescape_byte(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnyString<'t> {
    ByteString(ast::ByteString<'t>),
    CString(ast::CString<'t>),
    String(ast::String<'t>),
}

impl<'t> AnyString<'t> {
    pub fn value(self) -> Result<Cow<'t, str>, EscapeError> {
        fn from_utf8(s: Cow<'_, [u8]>) -> Result<Cow<'_, str>, EscapeError> {
            match s {
                Cow::Borrowed(s) => str::from_utf8(s)
                    .map_err(|_| EscapeError::NonAsciiCharInByte)
                    .map(Cow::Borrowed),
                // alloc: none, the owned bytes become the string in place.
                Cow::Owned(s) => std::string::String::from_utf8(s)
                    .map_err(|_| EscapeError::NonAsciiCharInByte)
                    .map(Cow::Owned),
            }
        }

        match self {
            AnyString::String(s) => s.value(),
            AnyString::ByteString(s) => s.value().and_then(from_utf8),
            AnyString::CString(s) => s.value().and_then(from_utf8),
        }
    }
}

impl<'t> AstToken<'t> for AnyString<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        ast::String::can_cast(kind)
            || ast::ByteString::can_cast(kind)
            || ast::CString::can_cast(kind)
    }

    fn cast(syntax: Token<'t>) -> Option<Self> {
        ast::String::cast(syntax)
            .map(Self::String)
            .or_else(|| ast::ByteString::cast(syntax).map(Self::ByteString))
            .or_else(|| ast::CString::cast(syntax).map(Self::CString))
    }

    fn syntax(&self) -> Token<'t> {
        match self {
            Self::ByteString(it) => it.syntax,
            Self::CString(it) => it.syntax,
            Self::String(it) => it.syntax,
        }
    }
}

impl<'t> IsString<'t> for AnyString<'t> {
    fn raw_prefix(&self) -> &'static str {
        match self {
            AnyString::ByteString(s) => s.raw_prefix(),
            AnyString::CString(s) => s.raw_prefix(),
            AnyString::String(s) => s.raw_prefix(),
        }
    }

    fn unescape(&self, s: &str, callback: impl FnMut(Range<usize>, Result<char, EscapeError>)) {
        match self {
            AnyString::ByteString(it) => it.unescape(s, callback),
            AnyString::CString(it) => it.unescape(s, callback),
            AnyString::String(it) => it.unescape(s, callback),
        }
    }
}

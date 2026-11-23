//! Pure `#![no_std]` HTML/XML entity codec, zero-allocation streaming tokenizer, tag stripper,
//! and strict allowlist HTML sanitizer.
//!
//! - **Entities**: [`escape_html_into`] / [`escape_html_in`] and [`unescape_html_into`] /
//!   [`unescape_html_in`] covering HTML5 named entities plus decimal (`&#NNNN;`) and hexadecimal
//!   (`&#xHHHH;` / `&#XHHHH;`) numeric character references with Unicode scalar validation.
//! - **Zero-alloc streaming tokenizer**: [`tokens`] yields borrowed [`Token`]s and [`attrs`]
//!   yields borrowed [`Attr`]s (double-quoted, single-quoted, unquoted, and boolean attributes).
//! - **Tag stripping & XSS-safe sanitization**: [`strip_tags_in`] and [`sanitize_html_in`] enforce
//!   explicit [`HtmlLimits`], drop raw-text and active-content elements (`<script>`, `<style>`,
//!   `<iframe>`, `<object>`, `<embed>`, `<svg>`, `<math>`), allow only safe formatting/structural
//!   tags and attributes, validate `href`/`src` URLs after entity decoding, and re-escape all
//!   output text and attribute values.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String};

/// Maximum length in bytes scanned between `&` and `;` when decoding an HTML entity.
const MAX_ENTITY_SCAN_BYTES: usize = 64;

/// Bounds enforced by [`strip_tags_in`] and [`sanitize_html_in`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HtmlLimits {
    /// Maximum input length in bytes.
    pub most_input_bytes: usize,
    /// Maximum output length in bytes.
    pub most_output_bytes: usize,
}

impl HtmlLimits {
    /// Default bounds: 4 MiB input, 8 MiB output.
    pub const USUAL: Self = Self {
        most_input_bytes: 4 << 20,
        most_output_bytes: 8 << 20,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed failure returned by [`strip_tags_in`] and [`sanitize_html_in`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HtmlError {
    /// Input byte length exceeded [`HtmlLimits::most_input_bytes`].
    InputTooLarge { bytes: usize, limit: usize },
    /// Output byte length would exceed [`HtmlLimits::most_output_bytes`].
    OutputTooLarge { bytes: usize, limit: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for HtmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HtmlError::InputTooLarge { bytes, limit } => {
                write!(f, "bstd::html: input size {bytes} bytes exceeds limit {limit}")
            }
            HtmlError::OutputTooLarge { bytes, limit } => {
                write!(f, "bstd::html: output size {bytes} bytes exceeds limit {limit}")
            }
            HtmlError::Alloc(err) => write!(f, "bstd::html: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for HtmlError {}

/// Escape HTML special characters (`&`, `<`, `>`, `"`, `'`) from `input` into `out`.
pub fn escape_html_into<W: fmt::Write>(input: &str, out: &mut W) -> fmt::Result {
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let replacement = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&#39;",
            _ => {
                i += 1;
                continue;
            }
        };
        if let Some(chunk) = input.get(start..i)
            && !chunk.is_empty()
        {
            out.write_str(chunk)?;
        }
        out.write_str(replacement)?;
        i += 1;
        start = i;
    }
    if let Some(tail) = input.get(start..)
        && !tail.is_empty()
    {
        out.write_str(tail)?;
    }
    Ok(())
}

/// Escape HTML special characters (`&`, `<`, `>`, `"`, `'`) into a freshly allocated [`String`].
pub fn escape_html_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut buf = String::try_with_capacity_in(input.len(), alloc)?;
    let mut writer = TryStringWriter { buf: &mut buf, err: None };
    if escape_html_into(input, &mut writer).is_err() {
        let len = writer.buf.len();
        return Err(writer.err.unwrap_or(AllocFailure::CapacityOverflow {
            what: "bstd::html::escape_html_in",
            len,
            additional: 1,
        }));
    }
    Ok(buf)
}

/// Decode common HTML5 named entities and decimal/hexadecimal numeric character references from
/// `input` into `out`, leaving unknown or malformed `&...` sequences literal.
pub fn unescape_html_into<W: fmt::Write>(input: &str, out: &mut W) -> fmt::Result {
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'&'
            && let Some(rest) = input.get(i..)
            && let Some((ch, consumed)) = decode_entity(rest)
        {
            if let Some(chunk) = input.get(start..i)
                && !chunk.is_empty()
            {
                out.write_str(chunk)?;
            }
            out.write_char(ch)?;
            i += consumed;
            start = i;
            continue;
        }
        i += 1;
    }
    if let Some(tail) = input.get(start..)
        && !tail.is_empty()
    {
        out.write_str(tail)?;
    }
    Ok(())
}

/// Decode HTML entities into a freshly allocated [`String`].
pub fn unescape_html_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut buf = String::try_with_capacity_in(input.len(), alloc)?;
    let mut writer = TryStringWriter { buf: &mut buf, err: None };
    if unescape_html_into(input, &mut writer).is_err() {
        let len = writer.buf.len();
        return Err(writer.err.unwrap_or(AllocFailure::CapacityOverflow {
            what: "bstd::html::unescape_html_in",
            len,
            additional: 1,
        }));
    }
    Ok(buf)
}

/// Attempt to decode an HTML entity starting at byte 0 (`&`) of `rest`.
/// Returns `(decoded_char, total_bytes_consumed_including_semicolon)`.
fn decode_entity(rest: &str) -> Option<(char, usize)> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'&') {
        return None;
    }
    let limit = bytes.len().min(MAX_ENTITY_SCAN_BYTES);
    let mut semi_pos = None;
    let mut idx = 1usize;
    while idx < limit {
        match bytes.get(idx) {
            Some(&b';') => {
                semi_pos = Some(idx);
                break;
            }
            Some(&b'&' | &b' ' | &b'\t' | &b'\n' | &b'\r' | &b'<') | None => return None,
            Some(_) => idx += 1,
        }
    }
    let semi = semi_pos?;
    let body = rest.get(1..semi)?;
    if body.is_empty() {
        return None;
    }
    let consumed = semi + 1;

    if let Some(num) = body.strip_prefix('#') {
        let cp = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            parse_hex_scalar(hex)?
        } else {
            parse_dec_scalar(num)?
        };
        let ch = char::from_u32(cp)?;
        return Some((ch, consumed));
    }

    let ch = lookup_named_entity(body)?;
    Some((ch, consumed))
}

fn parse_dec_scalar(digits: &str) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    for &b in digits.as_bytes() {
        let d = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            _ => return None,
        };
        acc = acc.saturating_mul(10).saturating_add(d);
        if acc > 0x10_FFFF {
            return None;
        }
    }
    Some(acc)
}

fn parse_hex_scalar(digits: &str) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    for &b in digits.as_bytes() {
        let d = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'a'..=b'f' => u32::from(b - b'a') + 10,
            b'A'..=b'F' => u32::from(b - b'A') + 10,
            _ => return None,
        };
        acc = acc.saturating_mul(16).saturating_add(d);
        if acc > 0x10_FFFF {
            return None;
        }
    }
    Some(acc)
}

fn lookup_named_entity(name: &str) -> Option<char> {
    match name {
        "amp" | "AMP" => Some('&'),
        "lt" | "LT" => Some('<'),
        "gt" | "GT" => Some('>'),
        "quot" | "QUOT" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "copy" | "COPY" => Some('\u{00A9}'),
        "reg" | "REG" => Some('\u{00AE}'),
        "trade" => Some('\u{2122}'),
        "mdash" => Some('\u{2014}'),
        "ndash" => Some('\u{2013}'),
        "hellip" => Some('\u{2026}'),
        "laquo" => Some('\u{00AB}'),
        "raquo" => Some('\u{00BB}'),
        "lsquo" => Some('\u{2018}'),
        "rsquo" => Some('\u{2019}'),
        "ldquo" => Some('\u{201C}'),
        "rdquo" => Some('\u{201D}'),
        "bull" => Some('\u{2022}'),
        "middot" => Some('\u{00B7}'),
        "times" => Some('\u{00D7}'),
        "divide" => Some('\u{00F7}'),
        "plusmn" => Some('\u{00B1}'),
        "deg" => Some('\u{00B0}'),
        "euro" => Some('\u{20AC}'),
        "pound" => Some('\u{00A3}'),
        "yen" => Some('\u{00A5}'),
        "cent" => Some('\u{00A2}'),
        "sect" => Some('\u{00A7}'),
        "colon" => Some(':'),
        "Tab" => Some('\t'),
        "NewLine" => Some('\n'),
        _ => None,
    }
}

/// One zero-copy HTML token yielded by [`tokens`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Token<'a> {
    /// Opening tag (`<tag attrs>` or `<tag attrs />`).
    StartTag { name: &'a str, attrs: &'a str, self_closing: bool },
    /// Closing tag (`</tag>`).
    EndTag { name: &'a str },
    /// Plain text slice between markup tokens.
    Text(&'a str),
    /// HTML comment body (`<!-- ... -->`).
    Comment(&'a str),
    /// Document type declaration body (`<!DOCTYPE ...>`).
    Doctype(&'a str),
}

/// Zero-allocation streaming iterator yielding [`Token`]s from an HTML slice.
#[derive(Clone, Copy, Debug)]
pub struct Tokens<'a> {
    rest: &'a str,
}

/// Create a zero-allocation streaming HTML tokenizer over `input`.
#[inline]
#[must_use]
pub const fn tokens(input: &str) -> Tokens<'_> {
    Tokens { rest: input }
}

impl<'a> Tokens<'a> {
    /// Advance past a raw-text body (`<script>` or `<style>`) until the matching `</tag>` closing
    /// tag (ASCII case-insensitive), or to EOF if unclosed.
    pub fn skip_raw_until(&mut self, tag: &str) {
        let bytes = self.rest.as_bytes();
        let tag_len = tag.len();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes.get(i) == Some(&b'<')
                && bytes.get(i + 1) == Some(&b'/')
                && let Some(candidate) = self.rest.get((i + 2)..(i + 2 + tag_len))
                && candidate.eq_ignore_ascii_case(tag)
            {
                let after_name = i + 2 + tag_len;
                let boundary_ok = match bytes.get(after_name) {
                    Some(&b'>' | &b'/' | &b' ' | &b'\t' | &b'\n' | &b'\r' | &0x0c) => true,
                    Some(_) | None => false,
                };
                if boundary_ok {
                    let mut j = after_name;
                    while let Some(&b) = bytes.get(j) {
                        if b == b'>' {
                            self.rest = self.rest.get((j + 1)..).unwrap_or("");
                            return;
                        }
                        j += 1;
                    }
                    self.rest = "";
                    return;
                }
            }
            i += 1;
        }
        self.rest = "";
    }

    fn consume_literal_lt(&mut self) -> Token<'a> {
        let tail_bytes = self.rest.as_bytes().get(1..).unwrap_or(&[]);
        let next_lt = tail_bytes.iter().position(|&b| b == b'<').map_or(self.rest.len(), |p| p + 1);
        let (head, tail) = self.rest.split_at_checked(next_lt).unwrap_or((self.rest, ""));
        self.rest = tail;
        Token::Text(head)
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.rest.as_bytes();
        let first = *bytes.first()?;
        if first != b'<' {
            let next_lt = bytes.iter().position(|&b| b == b'<').unwrap_or(self.rest.len());
            let (head, tail) = self.rest.split_at_checked(next_lt).unwrap_or((self.rest, ""));
            self.rest = tail;
            return Some(Token::Text(head));
        }

        // Comment: `<!-- ... -->`
        if self.rest.starts_with("<!--") {
            let after_open = self.rest.get(4..).unwrap_or("");
            if let Some(rel_end) = after_open.find("-->") {
                let body = after_open.get(..rel_end).unwrap_or("");
                self.rest = after_open.get((rel_end + 3)..).unwrap_or("");
                return Some(Token::Comment(body));
            }
            self.rest = "";
            return Some(Token::Comment(after_open));
        }

        // Doctype: `<!DOCTYPE ...>` (ASCII case-insensitive)
        if bytes.get(1) == Some(&b'!')
            && let Some(kw) = self.rest.get(2..9)
            && kw.eq_ignore_ascii_case("doctype")
        {
            let after_kw = self.rest.get(9..).unwrap_or("");
            if let Some(rel_end) = after_kw.as_bytes().iter().position(|&b| b == b'>') {
                let body = after_kw.get(..rel_end).unwrap_or("").trim();
                self.rest = after_kw.get((rel_end + 1)..).unwrap_or("");
                return Some(Token::Doctype(body));
            }
            self.rest = "";
            return Some(Token::Doctype(after_kw.trim()));
        }

        // End tag: `</name ...>`
        if bytes.get(1) == Some(&b'/')
            && bytes.get(2).is_some_and(u8::is_ascii_alphabetic)
        {
            let mut name_end = 2usize;
            while let Some(&b) = bytes.get(name_end) {
                if is_tag_name_byte(b) {
                    name_end += 1;
                } else {
                    break;
                }
            }
            let after_name = bytes.get(name_end..).unwrap_or(&[]);
            if let Some(rel_close) = after_name.iter().position(|&b| b == b'>') {
                let name = self.rest.get(2..name_end).unwrap_or("");
                let close = name_end + rel_close;
                self.rest = self.rest.get((close + 1)..).unwrap_or("");
                return Some(Token::EndTag { name });
            }
            return Some(self.consume_literal_lt());
        }

        // Start tag: `<name attrs>` or `<name attrs />`
        if bytes.get(1).is_some_and(u8::is_ascii_alphabetic) {
            let mut name_end = 1usize;
            while let Some(&b) = bytes.get(name_end) {
                if is_tag_name_byte(b) {
                    name_end += 1;
                } else {
                    break;
                }
            }
            if let Some(close) = find_tag_close(bytes, name_end) {
                let name = self.rest.get(1..name_end).unwrap_or("");
                let raw_attrs = self.rest.get(name_end..close).unwrap_or("").trim();
                let (attrs, self_closing) = match raw_attrs.strip_suffix('/') {
                    Some(stripped) => (stripped.trim(), true),
                    None => (raw_attrs, false),
                };
                self.rest = self.rest.get((close + 1)..).unwrap_or("");
                return Some(Token::StartTag { name, attrs, self_closing });
            }
            return Some(self.consume_literal_lt());
        }

        Some(self.consume_literal_lt())
    }
}

/// Scan for the closing `>` of a start tag starting at `from`, skipping `>` inside quoted
/// attribute values (`"..."` or `'...'` immediately after `=` and optional whitespace).
fn find_tag_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    let mut quote: Option<u8> = None;
    let mut after_eq = false;
    while let Some(&b) = bytes.get(i) {
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'>' {
            return Some(i);
        }
        if after_eq && (b == b'"' || b == b'\'') {
            quote = Some(b);
            after_eq = false;
        } else if b == b'=' {
            after_eq = true;
        } else if !is_html_space(b) {
            after_eq = false;
        }
        i += 1;
    }
    None
}

const fn is_tag_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':'
}

const fn is_html_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// One zero-copy parsed HTML attribute yielded by [`attrs`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attr<'a> {
    /// Attribute name as written in the source tag.
    pub name: &'a str,
    /// Attribute value (`None` for boolean attributes such as `disabled`).
    pub value: Option<&'a str>,
}

/// Zero-allocation iterator yielding [`Attr`]s from a start tag's raw attribute slice.
#[derive(Clone, Copy, Debug)]
pub struct Attrs<'a> {
    rest: &'a str,
}

/// Iterate over attributes inside `raw` (typically [`Token::StartTag`]'s `attrs` field).
#[inline]
#[must_use]
pub const fn attrs(raw: &str) -> Attrs<'_> {
    Attrs { rest: raw }
}

impl<'a> Iterator for Attrs<'a> {
    type Item = Attr<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let bytes = self.rest.as_bytes();
            let mut i = 0usize;
            while let Some(&b) = bytes.get(i) {
                if is_html_space(b) || b == b'/' {
                    i += 1;
                } else {
                    break;
                }
            }
            self.rest = self.rest.get(i..).unwrap_or("");
            let bytes = self.rest.as_bytes();
            if bytes.is_empty() {
                return None;
            }

            let mut name_end = 0usize;
            while let Some(&b) = bytes.get(name_end) {
                if is_html_space(b) || b == b'=' || b == b'/' || b == b'>' {
                    break;
                }
                name_end += 1;
            }
            if name_end == 0 {
                // Skip stray `=` or `>` to guarantee forward progress.
                self.rest = self.rest.get(1..).unwrap_or("");
                continue;
            }

            let name = self.rest.get(..name_end).unwrap_or("");
            let mut pos = name_end;
            while let Some(&b) = bytes.get(pos) {
                if is_html_space(b) {
                    pos += 1;
                } else {
                    break;
                }
            }

            if bytes.get(pos) != Some(&b'=') {
                self.rest = self.rest.get(pos..).unwrap_or("");
                return Some(Attr { name, value: None });
            }

            pos += 1; // consume `=`
            while let Some(&b) = bytes.get(pos) {
                if is_html_space(b) {
                    pos += 1;
                } else {
                    break;
                }
            }

            match bytes.get(pos) {
                Some(&q @ (b'"' | b'\'')) => {
                    let val_start = pos + 1;
                    let mut val_end = val_start;
                    while let Some(&b) = bytes.get(val_end) {
                        if b == q {
                            break;
                        }
                        val_end += 1;
                    }
                    let val = self.rest.get(val_start..val_end).unwrap_or("");
                    let next_pos = if bytes.get(val_end) == Some(&q) {
                        val_end + 1
                    } else {
                        val_end
                    };
                    self.rest = self.rest.get(next_pos..).unwrap_or("");
                    return Some(Attr { name, value: Some(val) });
                }
                Some(_) => {
                    let val_start = pos;
                    let mut val_end = val_start;
                    while let Some(&b) = bytes.get(val_end) {
                        if is_html_space(b) || b == b'>' {
                            break;
                        }
                        val_end += 1;
                    }
                    let val = self.rest.get(val_start..val_end).unwrap_or("");
                    self.rest = self.rest.get(val_end..).unwrap_or("");
                    return Some(Attr { name, value: Some(val) });
                }
                None => {
                    self.rest = "";
                    return Some(Attr { name, value: Some("") });
                }
            }
        }
    }
}

/// Strip all HTML tags, comments, doctypes, and `<script>`/`<style>` raw bodies, returning
/// entity-decoded visible text bounded by `limits`.
pub fn strip_tags_in<A: Allocator + Copy>(
    input: &str,
    limits: HtmlLimits,
    alloc: A,
) -> Result<String<A>, HtmlError> {
    if input.len() > limits.most_input_bytes {
        return Err(HtmlError::InputTooLarge {
            bytes: input.len(),
            limit: limits.most_input_bytes,
        });
    }

    let mut out = String::new_in(alloc);
    let mut writer = BoundedWriter::new(&mut out, limits.most_output_bytes);
    let mut toks = tokens(input);

    while let Some(tok) = toks.next() {
        match tok {
            Token::StartTag { name, self_closing, .. } => {
                if !self_closing
                    && (name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style"))
                {
                    toks.skip_raw_until(name);
                }
            }
            Token::Text(txt) => {
                if unescape_html_into(txt, &mut writer).is_err() {
                    return Err(writer.take_error());
                }
            }
            Token::EndTag { .. } | Token::Comment(_) | Token::Doctype(_) => {}
        }
    }

    Ok(out)
}

/// Sanitize untrusted HTML with a strict allowlist of safe formatting/structural tags, safe
/// attributes, entity-decoded URL scheme validation (`http:`, `https:`, `mailto:`, `#`, or
/// relative paths), and full re-escaping of all emitted text and attribute values.
pub fn sanitize_html_in<A: Allocator + Copy>(
    input: &str,
    limits: HtmlLimits,
    alloc: A,
) -> Result<String<A>, HtmlError> {
    sanitize_html_in_planted(input, limits, alloc, false)
}

fn sanitize_html_in_planted<A: Allocator + Copy>(
    input: &str,
    limits: HtmlLimits,
    alloc: A,
    plant_allow_javascript_url: bool,
) -> Result<String<A>, HtmlError> {
    if input.len() > limits.most_input_bytes {
        return Err(HtmlError::InputTooLarge {
            bytes: input.len(),
            limit: limits.most_input_bytes,
        });
    }

    let mut out = String::new_in(alloc);
    let mut scratch = String::new_in(alloc);
    let mut toks = tokens(input);

    // Active dropped container tag (`iframe`, `object`, `svg`, `math`) and its nesting depth.
    let mut drop_container: Option<&'static str> = None;
    let mut drop_depth = 0usize;

    while let Some(tok) = toks.next() {
        if let Some(active_drop) = drop_container {
            match tok {
                Token::StartTag { name, self_closing, .. } => {
                    if !self_closing && name.eq_ignore_ascii_case(active_drop) {
                        drop_depth = drop_depth.saturating_add(1);
                    }
                }
                Token::EndTag { name } => {
                    if name.eq_ignore_ascii_case(active_drop) {
                        drop_depth = drop_depth.saturating_sub(1);
                        if drop_depth == 0 {
                            drop_container = None;
                        }
                    }
                }
                Token::Text(_) | Token::Comment(_) | Token::Doctype(_) => {}
            }
            continue;
        }

        match tok {
            Token::StartTag { name, attrs: raw_attrs, self_closing } => {
                if name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style") {
                    if !self_closing {
                        toks.skip_raw_until(name);
                    }
                    continue;
                }
                if let Some(container) = dropped_container_tag(name) {
                    if !self_closing {
                        drop_container = Some(container);
                        drop_depth = 1;
                    }
                    continue;
                }
                if name.eq_ignore_ascii_case("embed") {
                    continue;
                }

                let Some(tag) = allowed_tag(name) else {
                    continue;
                };

                let mut writer = BoundedWriter::new(&mut out, limits.most_output_bytes);
                writer.push_char('<')?;
                writer.push_str(tag.name)?;

                let mut seen_mask = 0u8;
                for attr in attrs(raw_attrs) {
                    let Some(spec) = allowed_attr(tag.name, attr.name) else {
                        continue;
                    };
                    if (seen_mask & spec.bit) != 0 {
                        continue;
                    }
                    let Some(raw_val) = attr.value else {
                        continue;
                    };

                    scratch.clear();
                    {
                        let mut scratch_writer =
                            BoundedWriter::new(&mut scratch, limits.most_output_bytes);
                        if unescape_html_into(raw_val, &mut scratch_writer).is_err() {
                            return Err(scratch_writer.take_error());
                        }
                    }

                    let emit_val = if spec.is_url {
                        let trimmed = trim_ascii_whitespace(scratch.as_str());
                        if !is_safe_url(trimmed, plant_allow_javascript_url) {
                            continue;
                        }
                        trimmed
                    } else {
                        scratch.as_str()
                    };

                    seen_mask |= spec.bit;
                    writer.push_char(' ')?;
                    writer.push_str(spec.name)?;
                    writer.push_str("=\"")?;
                    if escape_html_into(emit_val, &mut writer).is_err() {
                        return Err(writer.take_error());
                    }
                    writer.push_char('"')?;
                }

                writer.push_char('>')?;
                if self_closing && !tag.is_void {
                    writer.push_str("</")?;
                    writer.push_str(tag.name)?;
                    writer.push_char('>')?;
                }
            }
            Token::EndTag { name } => {
                if let Some(tag) = allowed_tag(name)
                    && !tag.is_void
                {
                    let mut writer = BoundedWriter::new(&mut out, limits.most_output_bytes);
                    writer.push_str("</")?;
                    writer.push_str(tag.name)?;
                    writer.push_char('>')?;
                }
            }
            Token::Text(txt) => {
                scratch.clear();
                {
                    let mut scratch_writer =
                        BoundedWriter::new(&mut scratch, limits.most_output_bytes);
                    if unescape_html_into(txt, &mut scratch_writer).is_err() {
                        return Err(scratch_writer.take_error());
                    }
                }
                let mut writer = BoundedWriter::new(&mut out, limits.most_output_bytes);
                if escape_html_into(scratch.as_str(), &mut writer).is_err() {
                    return Err(writer.take_error());
                }
            }
            Token::Comment(_) | Token::Doctype(_) => {}
        }
    }

    Ok(out)
}

#[derive(Clone, Copy)]
struct AllowedTag {
    name: &'static str,
    is_void: bool,
}

fn allowed_tag(raw: &str) -> Option<AllowedTag> {
    const TAGS: &[AllowedTag] = &[
        AllowedTag { name: "p", is_void: false },
        AllowedTag { name: "br", is_void: true },
        AllowedTag { name: "hr", is_void: true },
        AllowedTag { name: "h1", is_void: false },
        AllowedTag { name: "h2", is_void: false },
        AllowedTag { name: "h3", is_void: false },
        AllowedTag { name: "h4", is_void: false },
        AllowedTag { name: "h5", is_void: false },
        AllowedTag { name: "h6", is_void: false },
        AllowedTag { name: "b", is_void: false },
        AllowedTag { name: "strong", is_void: false },
        AllowedTag { name: "i", is_void: false },
        AllowedTag { name: "em", is_void: false },
        AllowedTag { name: "u", is_void: false },
        AllowedTag { name: "s", is_void: false },
        AllowedTag { name: "del", is_void: false },
        AllowedTag { name: "code", is_void: false },
        AllowedTag { name: "pre", is_void: false },
        AllowedTag { name: "blockquote", is_void: false },
        AllowedTag { name: "ul", is_void: false },
        AllowedTag { name: "ol", is_void: false },
        AllowedTag { name: "li", is_void: false },
        AllowedTag { name: "table", is_void: false },
        AllowedTag { name: "thead", is_void: false },
        AllowedTag { name: "tbody", is_void: false },
        AllowedTag { name: "tr", is_void: false },
        AllowedTag { name: "th", is_void: false },
        AllowedTag { name: "td", is_void: false },
        AllowedTag { name: "a", is_void: false },
        AllowedTag { name: "img", is_void: true },
        AllowedTag { name: "span", is_void: false },
        AllowedTag { name: "div", is_void: false },
    ];
    for &tag in TAGS {
        if raw.eq_ignore_ascii_case(tag.name) {
            return Some(tag);
        }
    }
    None
}

fn dropped_container_tag(raw: &str) -> Option<&'static str> {
    const DROPPED: &[&str] = &["iframe", "object", "svg", "math"];
    for &tag in DROPPED {
        if raw.eq_ignore_ascii_case(tag) {
            return Some(tag);
        }
    }
    None
}

#[derive(Clone, Copy)]
struct AllowedAttr {
    name: &'static str,
    is_url: bool,
    bit: u8,
}

fn allowed_attr(canonical_tag: &str, raw_attr: &str) -> Option<AllowedAttr> {
    match canonical_tag {
        "a" => {
            if raw_attr.eq_ignore_ascii_case("href") {
                Some(AllowedAttr { name: "href", is_url: true, bit: 1 })
            } else if raw_attr.eq_ignore_ascii_case("title") {
                Some(AllowedAttr { name: "title", is_url: false, bit: 2 })
            } else {
                None
            }
        }
        "img" => {
            if raw_attr.eq_ignore_ascii_case("src") {
                Some(AllowedAttr { name: "src", is_url: true, bit: 1 })
            } else if raw_attr.eq_ignore_ascii_case("alt") {
                Some(AllowedAttr { name: "alt", is_url: false, bit: 2 })
            } else if raw_attr.eq_ignore_ascii_case("title") {
                Some(AllowedAttr { name: "title", is_url: false, bit: 4 })
            } else {
                None
            }
        }
        "th" | "td" => {
            if raw_attr.eq_ignore_ascii_case("align") {
                Some(AllowedAttr { name: "align", is_url: false, bit: 1 })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn trim_ascii_whitespace(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end && bytes.get(start).is_some_and(|&b| is_html_space(b)) {
        start += 1;
    }
    while end > start && bytes.get(end - 1).is_some_and(|&b| is_html_space(b)) {
        end -= 1;
    }
    s.get(start..end).unwrap_or("")
}

/// Validate an entity-decoded, whitespace-trimmed URL for `href`/`src`.
///
/// Permits only `http:`, `https:`, `mailto:`, `#` fragments, and relative paths with no explicit
/// scheme before `/`, `?`, or `#`. Refuses ASCII control characters and any other scheme
/// (`javascript:`, `vbscript:`, `data:`, etc.).
fn is_safe_url(decoded: &str, plant_allow_javascript_url: bool) -> bool {
    if plant_allow_javascript_url {
        return true;
    }
    let bytes = decoded.as_bytes();
    for &b in bytes {
        if b < 0x20 || b == 0x7f {
            return false;
        }
    }

    // Extract candidate scheme before the first `:`, `/`, `?`, or `#`.
    let mut scheme = [0u8; 16];
    let mut scheme_len = 0usize;
    let mut saw_colon = false;
    for &b in bytes {
        if b == b':' {
            saw_colon = true;
            break;
        }
        if b == b'/' || b == b'?' || b == b'#' {
            return true;
        }
        if b == b' ' {
            continue;
        }
        if let Some(slot) = scheme.get_mut(scheme_len) {
            *slot = b.to_ascii_lowercase();
            scheme_len += 1;
        } else {
            // Longer than any allowed scheme; if a `:` follows later, refuse.
            scheme_len = scheme.len() + 1;
        }
    }

    if !saw_colon {
        return true;
    }

    let Some(scheme_slice) = scheme.get(..scheme_len) else {
        return false;
    };
    scheme_slice == b"http" || scheme_slice == b"https" || scheme_slice == b"mailto"
}

struct TryStringWriter<'a, A: Allocator> {
    buf: &'a mut String<A>,
    err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.err.is_some() {
            return Err(fmt::Error);
        }
        if let Err(err) = self.buf.try_push_str(s) {
            self.err = Some(err);
            return Err(fmt::Error);
        }
        Ok(())
    }

    fn write_char(&mut self, ch: char) -> fmt::Result {
        if self.err.is_some() {
            return Err(fmt::Error);
        }
        if let Err(err) = self.buf.try_push(ch) {
            self.err = Some(err);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

struct BoundedWriter<'a, A: Allocator> {
    buf: &'a mut String<A>,
    limit: usize,
    err: Option<HtmlError>,
}

impl<'a, A: Allocator> BoundedWriter<'a, A> {
    fn new(buf: &'a mut String<A>, limit: usize) -> Self {
        Self { buf, limit, err: None }
    }

    fn push_str(&mut self, s: &str) -> Result<(), HtmlError> {
        if let Some(err) = self.err {
            return Err(err);
        }
        let next_len = self.buf.len().saturating_add(s.len());
        if next_len > self.limit {
            let err = HtmlError::OutputTooLarge { bytes: next_len, limit: self.limit };
            self.err = Some(err);
            return Err(err);
        }
        if let Err(err) = self.buf.try_push_str(s) {
            let wrapped = HtmlError::Alloc(err);
            self.err = Some(wrapped);
            return Err(wrapped);
        }
        Ok(())
    }

    fn push_char(&mut self, ch: char) -> Result<(), HtmlError> {
        let mut encoded = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut encoded))
    }

    fn take_error(&self) -> HtmlError {
        self.err.unwrap_or(HtmlError::OutputTooLarge {
            bytes: self.buf.len(),
            limit: self.limit,
        })
    }
}

impl<A: Allocator> fmt::Write for BoundedWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.push_str(s).is_err() { Err(fmt::Error) } else { Ok(()) }
    }

    fn write_char(&mut self, ch: char) -> fmt::Result {
        if self.push_char(ch).is_err() { Err(fmt::Error) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::{
        Attr, HtmlError, HtmlLimits, Token, attrs, escape_html_in, sanitize_html_in,
        sanitize_html_in_planted, strip_tags_in, tokens, unescape_html_in,
    };

    #[test]
    fn entity_escape_and_unescape_named_numeric_emoji_and_malformed() {
        let h = heap();
        let raw = r#"<a href="x&y">it's > 0</a>"#;
        let esc = escape_html_in(raw, h.alloc()).expect("escape");
        assert_eq!(esc.as_str(), "&lt;a href=&quot;x&amp;y&quot;&gt;it&#39;s &gt; 0&lt;/a&gt;");
        let back = unescape_html_in(esc.as_str(), h.alloc()).expect("unescape");
        assert_eq!(back.as_str(), raw);

        // Named entities.
        let named = unescape_html_in(
            "&amp;&lt;&gt;&quot;&apos;&#39;&nbsp;&copy;&reg;&trade;&mdash;&ndash;&hellip;&laquo;&raquo;",
            h.alloc(),
        )
        .expect("named");
        assert_eq!(named.as_str(), "&<>\"''\u{00A0}©®™—–…«»");

        // Decimal, lowercase/uppercase hex, leading zeros, and emoji.
        let numeric = unescape_html_in(
            "&#65;&#x42;&#X43;&#0000106;&#129408;&#x1F680;",
            h.alloc(),
        )
        .expect("numeric");
        assert_eq!(numeric.as_str(), "ABCj🦀🚀");

        // Malformed / unknown entities stay literal, while trailing valid entities still decode.
        let malformed = unescape_html_in(
            "&unknown; &#; &#x; &#9999999999; &#xD800; &a&lt; &amp",
            h.alloc(),
        )
        .expect("malformed");
        assert_eq!(malformed.as_str(), "&unknown; &#; &#x; &#9999999999; &#xD800; &a< &amp");
    }

    #[test]
    fn streaming_tokenizer_and_attrs_iterator() {
        let h = heap();
        let html = "<!DOCTYPE html><!-- hi --><div class=\"box\" data-x='a > b' width=42 disabled><br/>text &amp; 1 < 2</div>";
        let mut collected = OwnVec::new_in(h.alloc());
        for tok in tokens(html) {
            collected.push(tok);
        }
        assert_eq!(
            collected.as_slice(),
            &[
                Token::Doctype("html"),
                Token::Comment(" hi "),
                Token::StartTag {
                    name: "div",
                    attrs: "class=\"box\" data-x='a > b' width=42 disabled",
                    self_closing: false,
                },
                Token::StartTag { name: "br", attrs: "", self_closing: true },
                Token::Text("text &amp; 1 "),
                Token::Text("< 2"),
                Token::EndTag { name: "div" },
            ]
        );

        let Token::StartTag { attrs: raw_attrs, .. } = collected[2] else {
            panic!("expected div start tag");
        };
        let mut parsed_attrs = OwnVec::new_in(h.alloc());
        for a in attrs(raw_attrs) {
            parsed_attrs.push(a);
        }
        assert_eq!(
            parsed_attrs.as_slice(),
            &[
                Attr { name: "class", value: Some("box") },
                Attr { name: "data-x", value: Some("a > b") },
                Attr { name: "width", value: Some("42") },
                Attr { name: "disabled", value: None },
            ]
        );
    }

    #[test]
    fn strip_tags_drops_script_style_and_decodes_text() {
        let h = heap();
        let html = "<!DOCTYPE html><html><head><style>body { color: red; }</style></head><body><!-- secret --><script>if (a < b) { alert(1); }</script><p>Hello &amp; <b>world</b> &mdash; &#x1F525;</p></body></html>";
        let text = strip_tags_in(html, HtmlLimits::USUAL, h.alloc()).expect("strip");
        assert_eq!(text.as_str(), "Hello & world — 🔥");

        // Enforce input and output bounds.
        assert_eq!(
            strip_tags_in(
                "<p>hello</p>",
                HtmlLimits { most_input_bytes: 4, most_output_bytes: 64 },
                h.alloc(),
            ),
            Err(HtmlError::InputTooLarge { bytes: 12, limit: 4 })
        );
        assert_eq!(
            strip_tags_in(
                "<p>hello</p>",
                HtmlLimits { most_input_bytes: 64, most_output_bytes: 3 },
                h.alloc(),
            ),
            Err(HtmlError::OutputTooLarge { bytes: 5, limit: 3 })
        );
    }

    #[test]
    fn sanitizer_blocks_xss_vectors_and_preserves_safe_html() {
        let h = heap();

        // Safe formatting, tables, links, and images survive cleanly.
        let safe = "<p>Hi <strong>there</strong> &amp; <a href=\"https://ix.dev\" title=\"ix\" onclick=\"evil()\">link</a><img src=\"/logo.png\" alt=\"logo\" onerror=\"alert(1)\"><td align=\"right\">1</td></p>";
        let cleaned = sanitize_html_in(safe, HtmlLimits::SHIPPED, h.alloc()).expect("sanitize");
        assert_eq!(
            cleaned.as_str(),
            "<p>Hi <strong>there</strong> &amp; <a href=\"https://ix.dev\" title=\"ix\">link</a><img src=\"/logo.png\" alt=\"logo\"><td align=\"right\">1</td></p>"
        );

        // Dangerous tags and their bodies are completely dropped.
        let xss_tags = "<p>ok</p><script>alert(1)</script><style>body{background:url(javascript:alert(1))}</style><iframe src=\"https://evil\">x</iframe><svg><svg><script>bad()</script></svg></svg><math><mi>x</mi></math><object data=\"x\">y</object><embed src=\"x\">";
        let cleaned_tags =
            sanitize_html_in(xss_tags, HtmlLimits::SHIPPED, h.alloc()).expect("sanitize");
        assert_eq!(cleaned_tags.as_str(), "<p>ok</p>");

        // Obfuscated javascript:/vbscript:/data: URLs are stripped.
        let xss_urls = [
            "<a href=\"javascript:alert(1)\">a</a>",
            "<a href=\"  JaVaScRiPt:alert(1)\">a</a>",
            "<a href=\"&#x6a;avascript:alert(1)\">a</a>",
            "<a href=\"&#0000106;avascript:alert(1)\">a</a>",
            "<a href=\"java&#x09;script:alert(1)\">a</a>",
            "<a href=\"javascript&colon;alert(1)\">a</a>",
            "<a href=\"vbscript:msgbox(1)\">a</a>",
            "<a href=\"data:text/html,<script>alert(1)</script>\">a</a>",
            "<img src=\"javascript:alert(1)\" alt=\"pic\">",
        ];
        for vec in xss_urls {
            let out = sanitize_html_in(vec, HtmlLimits::SHIPPED, h.alloc()).expect("sanitize");
            let lower = ascii_lower_in(out.as_str(), h.alloc());
            assert!(
                !lower.as_str().contains("javascript:")
                    && !lower.as_str().contains("vbscript:")
                    && !lower.as_str().contains("data:")
                    && !lower.as_str().contains("<script"),
                "unsafe output for {vec:?}: {:?}",
                out.as_str()
            );
        }

        // Mailto, fragment, and relative URLs remain allowed.
        let good_urls = "<a href=\"mailto:hi@ix.dev\">m</a><a href=\"#top\">t</a><a href=\"docs/intro.html\">d</a>";
        let out_good =
            sanitize_html_in(good_urls, HtmlLimits::SHIPPED, h.alloc()).expect("sanitize");
        assert_eq!(out_good.as_str(), good_urls);
    }

    fn ascii_lower_in<A: Allocator>(s: &str, alloc: A) -> OwnString<A> {
        let mut out = OwnString::with_capacity_in(s.len(), alloc);
        for ch in s.chars() {
            out.push(ch.to_ascii_lowercase());
        }
        out
    }

    fn draw_utf8<A: Allocator>(tape: &Tape, alloc: A) -> OwnString<A> {
        const PIECES: &[&str] = &[
            "",
            "plain",
            "&",
            "<",
            ">",
            "\"",
            "'",
            "&amp;",
            "&lt;",
            "&#39;",
            "&#x6a;",
            "&unknown;",
            "<script>",
            "café",
            "日本語",
            "🦀",
            "\n\t\r",
        ];
        let count = tape.int_in(0usize..=10, "chunks");
        let mut s = OwnString::new_in(alloc);
        for _ in 0..count {
            s.push_str(PIECES[tape.index(PIECES.len(), "chunk")]);
        }
        s
    }

    fn draw_adversarial_html<A: Allocator>(tape: &Tape, alloc: A) -> OwnString<A> {
        const FRAGS: &[&str] = &[
            "<p>hello</p>",
            "<b>bold</b>",
            "<script>alert(1)</script>",
            "<SCRIPT>x < y</SCRIPT>",
            "<style>body{}</style>",
            "<iframe src=\"https://x\">inner</iframe>",
            "<svg><script>boom()</script></svg>",
            "<img src=\"https://ix.dev/a.png\" onerror=\"alert(1)\" alt=\"ok\">",
            "<img src=\"javascript:alert(1)\" onerror=alert(2)>",
            "<a href=\"javascript:alert(1)\" onclick=\"bad()\">click</a>",
            "<a href=\"&#x6a;avascript:alert(1)\">hex</a>",
            "<a href=\"java&#x09;script:alert(1)\">tab</a>",
            "<a href=\"javascript&colon;alert(1)\">colon</a>",
            "<a href=\"https://ix.dev\" title=\"ok\">safe</a>",
            "<!-- <script>hidden</script> -->",
            "plain &amp; text",
        ];
        let count = tape.int_in(1usize..=8, "frags");
        let mut html = OwnString::new_in(alloc);
        for _ in 0..count {
            html.push_str(FRAGS[tape.index(FRAGS.len(), "frag")]);
        }
        html
    }

    fn check_sanitized_invariants<A: Allocator + Copy>(
        html: &str,
        plant_allow_javascript_url: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let out = sanitize_html_in_planted(
            html,
            HtmlLimits::SHIPPED,
            alloc,
            plant_allow_javascript_url,
        )
        .map_err(|e| Broke::Oracle(format_in!(alloc, "sanitize failed: {e}").as_str().into()))?;
        let lower = ascii_lower_in(out.as_str(), alloc);
        for forbidden in ["<script", "onerror=", "javascript:"] {
            if lower.as_str().contains(forbidden) {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "forbidden `{forbidden}` leaked into sanitized output: {:?}",
                        out.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
        }
        Ok(())
    }

    /// The invariant: HTML entity escaping round-trips arbitrary UTF-8 and `sanitize_html_in` never emits `<script`, `onerror=`, or `javascript:`.
    #[test]
    fn property_escape_round_trip_and_sanitizer_never_leaks_xss() {
        let h = heap();
        check(Config::cases(256), |tape| {
            let text = draw_utf8(tape, h.alloc());
            let escaped = escape_html_in(text.as_str(), h.alloc()).expect("escape");
            let unescaped = unescape_html_in(escaped.as_str(), h.alloc()).expect("unescape");
            assert_eq!(unescaped.as_str(), text.as_str());

            let html = draw_adversarial_html(tape, h.alloc());
            if let Err(broke) = check_sanitized_invariants(html.as_str(), false, h.alloc()) {
                panic!("sanitizer invariant failed: {broke:?}");
            }
        });
    }

    #[test]
    fn the_oracle_catches_bypassed_javascript_url_check() {
        let h = heap();
        control(Config::cases(128), false, true, "", |tape, plant| {
            let html = draw_adversarial_html(tape, h.alloc());
            check_sanitized_invariants(html.as_str(), plant, h.alloc())
        });
    }
}

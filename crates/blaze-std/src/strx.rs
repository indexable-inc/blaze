//! Pure `#![no_std]` string transformation utilities:
//!
//! - Word splitting and case conversions ([`split_words`], [`to_snake_case_in`],
//!   [`to_screaming_snake_case_in`], [`to_kebab_case_in`], [`to_camel_case_in`],
//!   [`to_pascal_case_in`], [`to_title_case_in`]) with acronym and digit boundary awareness
//!   (`HTTPServer` -> `http_server`, `parseHTML5Doc` -> `parse_html5_doc`).
//! - URL slugification ([`slugify_in`]) using Unicode NFD decomposition (`blaze_unicode::nfd_chars`)
//!   to fold Latin diacritics cleanly.
//! - Zero-alloc natural sort comparison ([`natural_cmp`]) forming a strict total order over all
//!   `&str` with arbitrary-length decimal digit runs (never overflows `u64`).
//! - Indentation utilities ([`dedent_in`], [`indent_in`]) preserving line endings and blank lines.
//! - ANSI- and Unicode-display-width-aware truncation ([`truncate_width_in`]) over `crate::ansi`
//!   that never splits escape sequences or wide characters and closes active ANSI styles on
//!   truncation.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::cmp::Ordering;

use blaze_own::{AllocFailure, String};
use blaze_unicode::{combining_class, nfd_chars};

use crate::ansi::{Segment, char_width, has_ansi, segments, visible_width};

// ---------------------------------------------------------------------------
// Word splitting & case conversions
// ---------------------------------------------------------------------------

/// Zero-alloc iterator splitting an identifier or phrase into constituent words.
///
/// Splits on non-alphanumeric separators (`_`, `-`, whitespace, punctuation), camelCase
/// transitions (`fooBar` -> `foo`, `Bar`), digit-to-uppercase transitions (`parseHTML5Doc` ->
/// `parse`, `HTML5`, `Doc`), and acronym endings (`HTTPServer` -> `HTTP`, `Server`).
#[derive(Clone, Debug)]
pub struct Words<'a> {
    rest: &'a str,
}

/// Iterate over the words of `input` without allocating.
#[inline]
#[must_use]
pub const fn split_words(input: &str) -> Words<'_> {
    Words { rest: input }
}

impl<'a> Iterator for Words<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let mut iter = self.rest.char_indices();
        let start = loop {
            match iter.next() {
                Some((idx, ch)) if ch.is_alphanumeric() => break idx,
                Some(_) => {}
                None => {
                    self.rest = "";
                    return None;
                }
            }
        };
        let (_, tail) = self.rest.split_at_checked(start)?;
        self.rest = tail;

        let mut chars = self.rest.char_indices().peekable();
        let (_, mut prev) = chars.next()?;
        let mut end = prev.len_utf8();

        while let Some((idx, curr)) = chars.next() {
            if !curr.is_alphanumeric() {
                end = idx;
                break;
            }
            let next_ch = chars.peek().map(|&(_, c)| c);
            let boundary = (prev.is_lowercase() && curr.is_uppercase())
                || (prev.is_numeric() && curr.is_uppercase())
                || (prev.is_uppercase()
                    && curr.is_uppercase()
                    && next_ch.is_some_and(char::is_lowercase));
            if boundary {
                end = idx;
                break;
            }
            prev = curr;
            end = idx + curr.len_utf8();
        }

        let (word, rest) = self.rest.split_at_checked(end)?;
        self.rest = rest;
        Some(word)
    }
}

fn push_lowercase<A: Allocator>(word: &str, out: &mut String<A>) -> Result<(), AllocFailure> {
    for ch in word.chars() {
        for lc in ch.to_lowercase() {
            out.try_push(lc)?;
        }
    }
    Ok(())
}

fn push_uppercase<A: Allocator>(word: &str, out: &mut String<A>) -> Result<(), AllocFailure> {
    for ch in word.chars() {
        for uc in ch.to_uppercase() {
            out.try_push(uc)?;
        }
    }
    Ok(())
}

fn push_capitalized<A: Allocator>(word: &str, out: &mut String<A>) -> Result<(), AllocFailure> {
    let mut chars = word.chars();
    if let Some(first) = chars.next() {
        for uc in first.to_uppercase() {
            out.try_push(uc)?;
        }
        for ch in chars {
            for lc in ch.to_lowercase() {
                out.try_push(lc)?;
            }
        }
    }
    Ok(())
}

/// Convert `input` to `snake_case` in `alloc`.
pub fn to_snake_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push('_')?;
        }
        push_lowercase(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `SCREAMING_SNAKE_CASE` in `alloc`.
pub fn to_screaming_snake_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push('_')?;
        }
        push_uppercase(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `kebab-case` in `alloc`.
pub fn to_kebab_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push('-')?;
        }
        push_lowercase(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `camelCase` in `alloc`.
pub fn to_camel_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx == 0 {
            push_lowercase(word, &mut out)?;
        } else {
            push_capitalized(word, &mut out)?;
        }
    }
    Ok(out)
}

/// Convert `input` to `PascalCase` in `alloc`.
pub fn to_pascal_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for word in split_words(input) {
        push_capitalized(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `Title Case` in `alloc`.
pub fn to_title_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push(' ')?;
        }
        push_capitalized(word, &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// URL Slugify
// ---------------------------------------------------------------------------

#[inline]
fn is_combining_mark(ch: char) -> bool {
    if combining_class(ch) != 0 {
        return true;
    }
    matches!(
        u32::from(ch),
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

#[inline]
const fn fold_latin_special(ch: char) -> Option<&'static str> {
    match ch {
        'ß' => Some("ss"),
        'æ' | 'Æ' => Some("ae"),
        'œ' | 'Œ' => Some("oe"),
        'ø' | 'Ø' => Some("o"),
        'đ' | 'Đ' | 'ð' | 'Ð' => Some("d"),
        'ł' | 'Ł' => Some("l"),
        'þ' | 'Þ' => Some("th"),
        _ => None,
    }
}

/// Turn `input` into a clean ASCII URL slug using `separator` in `alloc`.
///
/// Decomposes characters via Unicode NFD (`blaze_unicode::nfd_chars`), strips combining marks,
/// folds common non-decomposing Latin letters (`ß`, `æ`, `œ`, `ø`, `đ`, `ł`, `þ`), lowercases
/// ASCII alphanumerics, collapses runs of separators, and trims leading/trailing separators.
pub fn slugify_in<A: Allocator + Copy>(
    input: &str,
    separator: char,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    let mut pending_sep = false;

    for ch in nfd_chars(input) {
        if is_combining_mark(ch) {
            continue;
        }
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.try_push(separator)?;
            }
            pending_sep = false;
            out.try_push(ch.to_ascii_lowercase())?;
        } else if let Some(folded) = fold_latin_special(ch) {
            if pending_sep && !out.is_empty() {
                out.try_push(separator)?;
            }
            pending_sep = false;
            out.try_push_str(folded)?;
        } else {
            pending_sep = true;
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Natural Sorting Comparison (strict total order, arbitrary-length digits)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum NaturalToken<'a> {
    Byte(u8),
    Digits(&'a [u8]),
}

fn next_natural_token(bytes: &[u8]) -> Option<(NaturalToken<'_>, &[u8])> {
    let first = *bytes.first()?;
    if first.is_ascii_digit() {
        let mut end = 1usize;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        let (run, rest) = bytes.split_at_checked(end)?;
        Some((NaturalToken::Digits(run), rest))
    } else {
        let (_, rest) = bytes.split_at_checked(1)?;
        Some((NaturalToken::Byte(first), rest))
    }
}

#[inline]
fn strip_leading_zeros(digits: &[u8]) -> &[u8] {
    let mut i = 0usize;
    while digits.get(i) == Some(&b'0') {
        i += 1;
    }
    digits.get(i..).unwrap_or(&[])
}

fn cmp_natural_tokens(
    a: NaturalToken<'_>,
    b: NaturalToken<'_>,
    plant_lexicographic_digits: bool,
) -> Ordering {
    match (a, b) {
        (NaturalToken::Byte(x), NaturalToken::Byte(y)) => x.cmp(&y),
        (NaturalToken::Byte(x), NaturalToken::Digits(_)) => {
            if x < b'0' {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (NaturalToken::Digits(_), NaturalToken::Byte(y)) => {
            if y < b'0' {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (NaturalToken::Digits(da), NaturalToken::Digits(db)) => {
            if plant_lexicographic_digits {
                da.cmp(db)
            } else {
                let ta = strip_leading_zeros(da);
                let tb = strip_leading_zeros(db);
                ta.len()
                    .cmp(&tb.len())
                    .then_with(|| ta.cmp(tb))
                    .then_with(|| da.len().cmp(&db.len()))
            }
        }
    }
}

fn natural_cmp_planted(a: &str, b: &str, plant_lexicographic_digits: bool) -> Ordering {
    let mut rem_a = a.as_bytes();
    let mut rem_b = b.as_bytes();

    loop {
        match (next_natural_token(rem_a), next_natural_token(rem_b)) {
            (Some((ta, next_a)), Some((tb, next_b))) => {
                let ord = cmp_natural_tokens(ta, tb, plant_lexicographic_digits);
                if ord != Ordering::Equal {
                    return ord;
                }
                rem_a = next_a;
                rem_b = next_b;
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Compare two strings in natural order without allocating.
///
/// Maximal runs of ASCII digits are compared by exact numeric value (stripping leading zeros,
/// comparing significant length then digits, and breaking numeric ties by original digit-run
/// length), so arbitrarily long numbers never overflow and the relation is a strict total order
/// (reflexive, antisymmetric, transitive) with `natural_cmp(a, b) == Equal` iff `a == b`.
#[must_use]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_cmp_planted(a, b, false)
}

// ---------------------------------------------------------------------------
// Indentation Utilities
// ---------------------------------------------------------------------------

struct LinesWithEndings<'a> {
    rest: &'a str,
}

#[inline]
const fn lines_with_endings(input: &str) -> LinesWithEndings<'_> {
    LinesWithEndings { rest: input }
}

impl<'a> Iterator for LinesWithEndings<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        if let Some(nl) = self.rest.as_bytes().iter().position(|&b| b == b'\n') {
            let (line_with_nl, after) = self.rest.split_at_checked(nl + 1)?;
            self.rest = after;
            if let Some(content) = line_with_nl.strip_suffix("\r\n") {
                Some((content, "\r\n"))
            } else if let Some(content) = line_with_nl.strip_suffix('\n') {
                Some((content, "\n"))
            } else {
                Some((line_with_nl, ""))
            }
        } else {
            let content = self.rest;
            self.rest = "";
            Some((content, ""))
        }
    }
}

#[inline]
fn is_blank_line(content: &str) -> bool {
    content.as_bytes().iter().all(|&b| b == b' ' || b == b'\t')
}

#[inline]
fn leading_ws(content: &str) -> &str {
    let bytes = content.as_bytes();
    let mut n = 0usize;
    while bytes.get(n).is_some_and(|&b| b == b' ' || b == b'\t') {
        n += 1;
    }
    content.split_at_checked(n).map_or("", |(ws, _)| ws)
}

#[inline]
fn common_ws_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let mut n = 0usize;
    for (&x, &y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        if x == y {
            n += 1;
        } else {
            break;
        }
    }
    a.split_at_checked(n).map_or("", |(p, _)| p)
}

/// Strip the common leading space/tab indentation across all non-empty lines in `alloc`.
///
/// Preserves line endings (`\n` and `\r\n`) and keeps blank lines empty.
pub fn dedent_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut common: Option<&str> = None;
    for (content, _) in lines_with_endings(input) {
        if is_blank_line(content) {
            continue;
        }
        let ws = leading_ws(content);
        common = Some(match common {
            None => ws,
            Some(prev) => common_ws_prefix(prev, ws),
        });
    }
    let strip_len = common.map_or(0, str::len);

    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (content, ending) in lines_with_endings(input) {
        if !is_blank_line(content) {
            let trimmed = content.split_at_checked(strip_len).map_or(content, |(_, tail)| tail);
            out.try_push_str(trimmed)?;
        }
        out.try_push_str(ending)?;
    }
    Ok(out)
}

/// Prepend `prefix` to every non-blank line of `input` in `alloc`, preserving line endings and
/// blank lines.
pub fn indent_in<A: Allocator + Copy>(
    input: &str,
    prefix: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (content, ending) in lines_with_endings(input) {
        if !is_blank_line(content) {
            out.try_push_str(prefix)?;
        }
        out.try_push_str(content)?;
        out.try_push_str(ending)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ANSI / Unicode Display-Width Truncation
// ---------------------------------------------------------------------------

fn push_width_prefix_in<A: Allocator>(
    src: &str,
    budget: usize,
    out: &mut String<A>,
) -> Result<bool, AllocFailure> {
    if budget == 0 {
        return Ok(false);
    }
    let mut used = 0usize;
    let mut emitted_escape = false;
    let mut stopped = false;

    for seg in segments(src) {
        if stopped {
            break;
        }
        match seg {
            Segment::Escape(esc) => {
                if used < budget {
                    out.try_push_str(esc)?;
                    emitted_escape = true;
                }
            }
            Segment::Text(txt) => {
                for ch in txt.chars() {
                    let cw = char_width(ch);
                    if used.saturating_add(cw) > budget {
                        stopped = true;
                        break;
                    }
                    out.try_push(ch)?;
                    used += cw;
                }
            }
        }
    }

    Ok(emitted_escape)
}

/// Truncate `input` so its visible terminal display width is at most `max_width`, appending
/// `ellipsis` when truncated.
///
/// Uses `crate::ansi::{segments, char_width, visible_width}`: never splits inside an ANSI escape
/// sequence or a wide East Asian / emoji scalar, and appends `\x1b[0m` if any escape sequence was
/// emitted before truncating so styling never bleeds.
pub fn truncate_width_in<A: Allocator + Copy>(
    input: &str,
    max_width: usize,
    ellipsis: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    if visible_width(input) <= max_width {
        return String::try_from_str_in(input, alloc);
    }
    if max_width == 0 {
        return Ok(String::new_in(alloc));
    }

    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width > max_width {
        let mut out = String::new_in(alloc);
        let esc = push_width_prefix_in(ellipsis, max_width, &mut out)?;
        if esc && !out.as_str().ends_with("\x1b[0m") {
            out.try_push_str("\x1b[0m")?;
        }
        return Ok(out);
    }

    let budget = max_width - ellipsis_width;
    let cap = input
        .len()
        .min(max_width.saturating_mul(4))
        .saturating_add(ellipsis.len())
        .saturating_add(8);
    let mut out = String::try_with_capacity_in(cap, alloc)?;

    let esc_in = push_width_prefix_in(input, budget, &mut out)?;
    let esc_el = has_ansi(ellipsis);
    out.try_push_str(ellipsis)?;
    if (esc_in || esc_el) && !out.as_str().ends_with("\x1b[0m") {
        out.try_push_str("\x1b[0m")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape};

    use super::*;

    #[test]
    fn case_conversion_edge_cases() {
        let h = heap();
        let alloc = h.alloc();

        assert_eq!(to_snake_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("http_server"));
        assert_eq!(to_screaming_snake_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("HTTP_SERVER"));
        assert_eq!(to_kebab_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("http-server"));
        assert_eq!(to_camel_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("httpServer"));
        assert_eq!(to_pascal_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("HttpServer"));
        assert_eq!(to_title_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("Http Server"));

        assert_eq!(to_snake_case_in("parseHTML5Doc", alloc).as_ref().map(OwnString::as_str), Ok("parse_html5_doc"));
        assert_eq!(to_camel_case_in("parseHTML5Doc", alloc).as_ref().map(OwnString::as_str), Ok("parseHtml5Doc"));
        assert_eq!(to_pascal_case_in("parseHTML5Doc", alloc).as_ref().map(OwnString::as_str), Ok("ParseHtml5Doc"));

        assert_eq!(to_snake_case_in("getURLPath", alloc).as_ref().map(OwnString::as_str), Ok("get_url_path"));
        assert_eq!(to_kebab_case_in("getURLPath", alloc).as_ref().map(OwnString::as_str), Ok("get-url-path"));
        assert_eq!(to_title_case_in("getURLPath", alloc).as_ref().map(OwnString::as_str), Ok("Get Url Path"));

        assert_eq!(to_snake_case_in("already_snake", alloc).as_ref().map(OwnString::as_str), Ok("already_snake"));
        assert_eq!(to_camel_case_in("already_snake", alloc).as_ref().map(OwnString::as_str), Ok("alreadySnake"));
        assert_eq!(to_pascal_case_in("foo--bar", alloc).as_ref().map(OwnString::as_str), Ok("FooBar"));
        assert_eq!(to_snake_case_in("foo--bar", alloc).as_ref().map(OwnString::as_str), Ok("foo_bar"));
        assert_eq!(to_snake_case_in("XMLHttpRequest", alloc).as_ref().map(OwnString::as_str), Ok("xml_http_request"));
        assert_eq!(to_snake_case_in("___---", alloc).as_ref().map(OwnString::as_str), Ok(""));
    }

    #[test]
    fn slugify_diacritics_and_separators() {
        let h = heap();
        let alloc = h.alloc();

        assert_eq!(
            slugify_in("Café & Crème Brûlée!", '-', alloc).as_ref().map(OwnString::as_str),
            Ok("cafe-creme-brulee")
        );
        assert_eq!(
            slugify_in("  --- Hello, World! 2026 ---  ", '_', alloc)
                .as_ref()
                .map(OwnString::as_str),
            Ok("hello_world_2026")
        );
        assert_eq!(
            slugify_in("Über Straße & Cœur", '-', alloc).as_ref().map(OwnString::as_str),
            Ok("uber-strasse-coeur")
        );
        assert_eq!(
            slugify_in("!!!", '-', alloc).as_ref().map(OwnString::as_str),
            Ok("")
        );
    }

    #[test]
    fn natural_cmp_ordering_and_huge_numbers() {
        let h = heap();
        let alloc = h.alloc();
        let mut items = OwnVec::new_in(alloc);
        for s in ["v100", "v2", "v10", "v1", "v1.10", "v1.2", "v1.1"] {
            items.push(s);
        }
        items.sort_unstable_by(|a, b| natural_cmp(a, b));
        assert_eq!(items.as_slice(), &["v1", "v1.1", "v1.2", "v1.10", "v2", "v10", "v100"]);

        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("0", "00"), Ordering::Less);
        assert_eq!(natural_cmp("00", "1"), Ordering::Less);
        assert_eq!(natural_cmp("1", "01"), Ordering::Less);

        // 100-digit integers never overflow u64.
        let mut huge_a = OwnString::new_in(alloc);
        let mut huge_b = OwnString::new_in(alloc);
        let mut huge_c = OwnString::new_in(alloc);
        huge_a.push('n');
        huge_b.push('n');
        huge_c.push_str("n1");
        for _ in 0..99 {
            huge_a.push('9');
            huge_b.push('9');
            huge_c.push('0');
        }
        huge_a.push('8');
        huge_b.push('9');
        huge_c.push('0');
        assert_eq!(natural_cmp(huge_a.as_str(), huge_b.as_str()), Ordering::Less);
        assert_eq!(natural_cmp(huge_b.as_str(), huge_c.as_str()), Ordering::Less);
        assert_eq!(natural_cmp(huge_a.as_str(), huge_c.as_str()), Ordering::Less);
    }

    #[test]
    fn dedent_and_indent_preserve_blank_lines_and_endings() {
        let h = heap();
        let alloc = h.alloc();

        let src = "    fn main() {\n        println!(\"hi\");\n    \n    }\n";
        let dedented = dedent_in(src, alloc);
        assert_eq!(
            dedented.as_ref().map(OwnString::as_str),
            Ok("fn main() {\n    println!(\"hi\");\n\n}\n")
        );

        let crlf = "\t\ta\r\n\t\t\r\n\tb";
        assert_eq!(dedent_in(crlf, alloc).as_ref().map(OwnString::as_str), Ok("\ta\r\n\r\nb"));

        let indented = indent_in("a\n\nb\r\nc", "  ", alloc);
        assert_eq!(indented.as_ref().map(OwnString::as_str), Ok("  a\n\n  b\r\n  c"));

        if let Ok(ind) = indented {
            assert_eq!(dedent_in(ind.as_str(), alloc).as_ref().map(OwnString::as_str), Ok("a\n\nb\r\nc"));
        }
    }

    #[test]
    fn truncate_width_ansi_and_cjk() {
        let h = heap();
        let alloc = h.alloc();

        // Fits verbatim without truncation or added reset.
        let short = "\x1b[31mhi\x1b[0m";
        assert_eq!(
            truncate_width_in(short, 5, "…", alloc).as_ref().map(OwnString::as_str),
            Ok(short)
        );

        // Truncated ANSI string closes color with \x1b[0m.
        let colored = "\x1b[31mhello world\x1b[0m";
        let cut = truncate_width_in(colored, 6, "…", alloc);
        assert_eq!(cut.as_ref().map(OwnString::as_str), Ok("\x1b[31mhello…\x1b[0m"));
        if let Ok(s) = cut {
            assert_eq!(visible_width(s.as_str()), 6);
        }

        // Wide CJK chars never split mid-glyph.
        let cjk = "\x1b[32m日本語テスト\x1b[0m";
        let cut_cjk = truncate_width_in(cjk, 5, "…", alloc);
        assert_eq!(cut_cjk.as_ref().map(OwnString::as_str), Ok("\x1b[32m日本…\x1b[0m"));
        if let Ok(s) = cut_cjk {
            assert_eq!(visible_width(s.as_str()), 5);
        }

        // Odd width with 2-cell CJK leaves width 3 <= 4.
        let odd_cjk = truncate_width_in("日本語", 4, "…", alloc);
        assert_eq!(odd_cjk.as_ref().map(OwnString::as_str), Ok("日…"));
        if let Ok(s) = odd_cjk {
            assert!(visible_width(s.as_str()) <= 4);
        }

        // Edge cases: max_width == 0 and ellipsis wider than max_width.
        assert_eq!(truncate_width_in("hello", 0, "…", alloc).as_ref().map(OwnString::as_str), Ok(""));
        assert_eq!(truncate_width_in("hello", 2, "...", alloc).as_ref().map(OwnString::as_str), Ok(".."));
    }

    fn draw_natural_str<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
        const PIECES: &[&str] = &[
            "a", "b", "0", "00", "1", "01", "2", "10", "99", "100", "-", ".", "/", "_", ":",
        ];
        let count = tape.int_in(0usize..=5, "pieces");
        let mut s = OwnString::new_in(alloc);
        for _ in 0..count {
            let p = tape.pick(PIECES, "piece").copied().unwrap_or("0");
            s.push_str(p);
        }
        s
    }

    /// The invariant: `natural_cmp` is a strict total order (reflexive, antisymmetric, transitive) and orders embedded decimal integers numerically.
    #[test]
    fn property_natural_cmp_strict_total_order() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            let a = draw_natural_str(tape, alloc);
            let b = draw_natural_str(tape, alloc);
            let c = draw_natural_str(tape, alloc);

            // Reflexivity.
            if natural_cmp(a.as_str(), a.as_str()) != Ordering::Equal {
                return Err(Broke::Oracle(format_in!(alloc, "reflexivity failed for {:?}", a.as_str()).as_str().into()));
            }
            // Antisymmetry & equality identity.
            let ab = natural_cmp(a.as_str(), b.as_str());
            let ba = natural_cmp(b.as_str(), a.as_str());
            if ab != ba.reverse() {
                return Err(Broke::Oracle(
                    format_in!(alloc, "antisymmetry failed for {:?} vs {:?}", a.as_str(), b.as_str())
                        .as_str()
                        .into(),
                ));
            }
            if (ab == Ordering::Equal) != (a.as_str() == b.as_str()) {
                return Err(Broke::Oracle(
                    format_in!(alloc, "equality identity failed for {:?} vs {:?}", a.as_str(), b.as_str())
                        .as_str()
                        .into(),
                ));
            }

            // Transitivity across all 3 pairs.
            let bc = natural_cmp(b.as_str(), c.as_str());
            let ac = natural_cmp(a.as_str(), c.as_str());
            if ab == Ordering::Less && bc == Ordering::Less && ac != Ordering::Less {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "transitivity failed: {:?} < {:?} < {:?}",
                        a.as_str(),
                        b.as_str(),
                        c.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
            if ab == Ordering::Greater && bc == Ordering::Greater && ac != Ordering::Greater {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "reverse transitivity failed: {:?} > {:?} > {:?}",
                        a.as_str(),
                        b.as_str(),
                        c.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }

            // Numeric agreement on embedded integers.
            let x = tape.int_in(0u64..=500u64, "x");
            let y = tape.int_in(0u64..=500u64, "y");
            let sx = format_in!(alloc, "item{x}.txt");
            let sy = format_in!(alloc, "item{y}.txt");
            if natural_cmp(sx.as_str(), sy.as_str()) != x.cmp(&y) {
                return Err(Broke::Oracle(
                    format_in!(alloc, "numeric mismatch for {sx} vs {sy}").as_str().into(),
                ));
            }
            Ok(())
        });
    }

    /// The invariant: `truncate_width_in` always produces a string with `visible_width <= max_width` and closes any ANSI style emitted before truncating.
    #[test]
    fn property_truncate_width_respects_bound() {
        let h = heap();
        const CHUNKS: &[&str] = &[
            "a",
            "hello",
            "日",
            "本語",
            "e\u{0301}",
            "\x1b[31m",
            "\x1b[1;34m",
            "\x1b[0m",
            " ",
        ];
        const ELLIPSES: &[&str] = &["…", "...", "", "——"];

        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            let count = tape.int_in(0usize..=10, "chunks");
            let mut input = OwnString::new_in(alloc);
            for _ in 0..count {
                let c = tape.pick(CHUNKS, "chunk").copied().unwrap_or("a");
                input.push_str(c);
            }
            let max_width = tape.int_in(0usize..=20, "max_width");
            let ellipsis = tape.pick(ELLIPSES, "ellipsis").copied().unwrap_or("…");

            let out = truncate_width_in(input.as_str(), max_width, ellipsis, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "truncate alloc: {e}").as_str().into()))?;
            let got_width = visible_width(out.as_str());
            if got_width > max_width {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "visible width {got_width} exceeded max_width {max_width} for {:?}",
                        out.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
            if visible_width(input.as_str()) <= max_width && out.as_str() != input.as_str() {
                return Err(Broke::Oracle(
                    format_in!(alloc, "non-truncated input was altered").as_str().into(),
                ));
            }
            if visible_width(input.as_str()) > max_width
                && has_ansi(out.as_str())
                && !out.as_str().ends_with("\x1b[0m")
            {
                return Err(Broke::Oracle(
                    format_in!(alloc, "truncated ANSI output leaked style: {:?}", out.as_str())
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_lexicographic_digit_bug() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let alloc = h.alloc();
            let x = tape.int_in(0u64..=200u64, "x");
            let y = tape.int_in(0u64..=200u64, "y");
            let a = format_in!(alloc, "file{x}");
            let b = format_in!(alloc, "file{y}");

            let got = natural_cmp_planted(a.as_str(), b.as_str(), plant);
            let want = x.cmp(&y);
            if got != want {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "natural_cmp({:?}, {:?}) returned {:?}, expected {:?}",
                        a.as_str(),
                        b.as_str(),
                        got,
                        want
                    )
                    .as_str()
                    .into(),
                ));
            }
            Ok(())
        });
    }
}

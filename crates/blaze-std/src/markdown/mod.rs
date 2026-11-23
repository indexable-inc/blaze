//! Bounded, iterative, pure `#![no_std]` CommonMark + GFM subset parser and dual HTML / ANSI
//! terminal renderer.
//!
//! - Zero recursion, zero `unsafe`, zero integer `as` casts, zero `.clone()`, and zero panicking
//!   slice indexes.
//! - Flat arena/Vec-backed AST ([`Document`]) storing [`Block`]s and [`Inline`]s in two contiguous
//!   vectors inside the caller's allocator, bounded by [`MarkdownLimits`].
//! - Dual rendering: XSS-safe HTML ([`to_html_in`], [`Document::render_html_into`]) and styled
//!   terminal ANSI output ([`to_ansi_in`], [`Document::render_ansi_into`]) using [`crate::ansi`]
//!   and [`crate::table`].

mod render_ansi;
mod render_html;
#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::fmt;
use core::ops::Range;

use blaze_own::{AllocFailure, String, Vec};

use crate::table::Align;

/// Explicit resource bounds enforced while parsing untrusted Markdown input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MarkdownLimits {
    /// Maximum accepted input size in bytes.
    pub most_bytes: usize,
    /// Maximum number of top-level blocks.
    pub most_blocks: usize,
    /// Maximum total number of inline nodes across the entire document.
    pub most_inlines: usize,
}

impl MarkdownLimits {
    /// Generous default bounds (16 MiB input, 100,000 blocks, 1,000,000 inlines).
    pub const USUAL: Self = Self {
        most_bytes: 16 << 20,
        most_blocks: 100_000,
        most_inlines: 1_000_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED` and `CsvLimits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Why parsing or rendering Markdown failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkdownError {
    /// Input byte length exceeded [`MarkdownLimits::most_bytes`].
    InputTooLarge { bytes: usize, limit: usize },
    /// Document exceeded [`MarkdownLimits::most_blocks`].
    TooManyBlocks { limit: usize },
    /// Document exceeded [`MarkdownLimits::most_inlines`].
    TooManyInlines { limit: usize },
    /// The caller's allocator refused memory.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for MarkdownError {
    #[inline]
    fn from(why: AllocFailure) -> Self {
        Self::Alloc(why)
    }
}

impl fmt::Display for MarkdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkdownError::InputTooLarge { bytes, limit } => {
                write!(f, "markdown input of {bytes} bytes exceeds limit of {limit} bytes")
            }
            MarkdownError::TooManyBlocks { limit } => {
                write!(f, "markdown document exceeds block limit of {limit}")
            }
            MarkdownError::TooManyInlines { limit } => {
                write!(f, "markdown document exceeds inline limit of {limit}")
            }
            MarkdownError::Alloc(why) => write!(f, "markdown allocation failed: {why}"),
        }
    }
}

impl core::error::Error for MarkdownError {}

/// Flat, non-recursive parsed Markdown document.
///
/// All inline nodes across headings, paragraphs, list items, and table cells live contiguously in
/// [`Document::inlines`]; each [`Block`] references its slice via a `Range<usize>`.
#[derive(Debug, PartialEq, Eq)]
pub struct Document<'a, A: Allocator> {
    pub blocks: Vec<Block<'a, A>, A>,
    pub inlines: Vec<Inline<'a>, A>,
}

impl<'a, A: Allocator> Document<'a, A> {
    /// Borrow the inline nodes referenced by `range`, returning an empty slice if out of bounds.
    #[inline]
    #[must_use]
    pub fn inlines_for(&self, range: Range<usize>) -> &[Inline<'a>] {
        self.inlines.as_slice().get(range).unwrap_or(&[])
    }
}

/// Top-level Markdown block stored flat inside [`Document::blocks`].
#[derive(Debug, PartialEq, Eq)]
pub enum Block<'a, A: Allocator> {
    /// ATX heading (`#` through `######`).
    Heading { level: u8, inlines: Range<usize> },
    /// Paragraph of one or more contiguous lines.
    Paragraph { inlines: Range<usize> },
    /// Fenced code block (```` ``` ```` or `~~~`) borrowing its raw body zero-copy.
    CodeBlock { lang: Option<&'a str>, code: &'a str },
    /// Block quote lines (`> ...`), stored flat without recursion.
    BlockQuote { lines: Vec<&'a str, A> },
    /// Ordered (`1.`) or unordered (`-`, `*`, `+`) list; each item is a range into [`Document::inlines`].
    List { ordered: bool, start: u32, items: Vec<Range<usize>, A> },
    /// Thematic break (`---`, `***`, `___`).
    ThematicBreak,
    /// GitHub-Flavored Markdown pipe table.
    Table {
        headers: Vec<Range<usize>, A>,
        aligns: Vec<Align, A>,
        rows: Vec<Vec<Range<usize>, A>, A>,
    },
}

/// Inline Markdown node stored flat inside [`Document::inlines`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline<'a> {
    Text(&'a str),
    Code(&'a str),
    Bold(&'a str),
    Italic(&'a str),
    Strikethrough(&'a str),
    Link { text: &'a str, url: &'a str },
    Image { alt: &'a str, url: &'a str },
    SoftBreak,
    HardBreak,
}

/// Fallible `fmt::Write` adapter backed by [`blaze_own::String`].
pub(super) struct TryStringWriter<'a, A: Allocator> {
    pub(super) out: &'a mut String<A>,
    pub(super) err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self.out.try_push_str(s) {
            Ok(()) => Ok(()),
            Err(why) => {
                self.err = Some(why);
                Err(fmt::Error)
            }
        }
    }

    fn write_char(&mut self, ch: char) -> fmt::Result {
        match self.out.try_push(ch) {
            Ok(()) => Ok(()),
            Err(why) => {
                self.err = Some(why);
                Err(fmt::Error)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    text_end: usize,
    next_pos: usize,
}

#[derive(Clone, Copy)]
struct LineCursor<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> LineCursor<'a> {
    const fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<Line<'a>> {
        let rest = self.input.get(self.pos..)?;
        if rest.is_empty() {
            return None;
        }
        let (text_end, next_pos) = match rest.as_bytes().iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let line_end = self.pos + rel;
                let trimmed_end = if line_end > self.pos
                    && self.input.as_bytes().get(line_end - 1) == Some(&b'\r')
                {
                    line_end - 1
                } else {
                    line_end
                };
                (trimmed_end, line_end + 1)
            }
            None => (self.input.len(), self.input.len()),
        };
        let text = self.input.get(self.pos..text_end).unwrap_or("");
        Some(Line { text, text_end, next_pos })
    }

    fn next_line(&mut self) -> Option<Line<'a>> {
        let line = self.peek()?;
        self.pos = line.next_pos;
        Some(line)
    }

    fn peek_second(&self) -> Option<Line<'a>> {
        let first = self.peek()?;
        let next_cur = LineCursor { input: self.input, pos: first.next_pos };
        next_cur.peek()
    }
}

/// Strip up to 3 leading ASCII space characters (CommonMark non-indented block prefix).
fn strip_indent_up_to_3(line: &str) -> &str {
    let spaces = line.as_bytes().iter().take(3).take_while(|&&b| b == b' ').count();
    line.get(spaces..).unwrap_or("")
}

fn is_blank(line: &str) -> bool {
    line.bytes().all(|b| b == b' ' || b == b'\t' || b == b'\r')
}

/// Match opening code fence (` ``` ` or `~~~`), returning `(fence_byte, fence_len, lang)`.
fn parse_fence_open(line: &str) -> Option<(u8, usize, Option<&str>)> {
    let trimmed = strip_indent_up_to_3(line);
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let fence_len = trimmed.as_bytes().iter().take_while(|&&b| b == first).count();
    if fence_len < 3 {
        return None;
    }
    let info = trimmed.get(fence_len..).unwrap_or("").trim();
    if first == b'`' && info.contains('`') {
        return None;
    }
    let lang = info.split_whitespace().next();
    Some((first, fence_len, lang))
}

fn is_fence_close(line: &str, fence_byte: u8, min_len: usize) -> bool {
    let trimmed = strip_indent_up_to_3(line);
    let count = trimmed.as_bytes().iter().take_while(|&&b| b == fence_byte).count();
    if count < min_len {
        return false;
    }
    let rest = trimmed.get(count..).unwrap_or("");
    rest.bytes().all(|b| b == b' ' || b == b'\t')
}

fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = strip_indent_up_to_3(line);
    let hashes = trimmed.as_bytes().iter().take_while(|&&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = trimmed.get(hashes..).unwrap_or("");
    if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }
    let content = after.trim_matches([' ', '\t']);
    let stripped = content.trim_end_matches('#');
    let text = if stripped.len() < content.len()
        && (stripped.is_empty() || stripped.ends_with(' ') || stripped.ends_with('\t'))
    {
        stripped.trim_end_matches([' ', '\t'])
    } else {
        content
    };
    let level = u8::try_from(hashes).ok()?;
    Some((level, text))
}

fn is_thematic_break(line: &str) -> bool {
    let trimmed = strip_indent_up_to_3(line);
    let mut marker: Option<u8> = None;
    let mut count = 0usize;
    for &b in trimmed.as_bytes() {
        if b == b' ' || b == b'\t' {
            continue;
        }
        if b != b'-' && b != b'*' && b != b'_' {
            return false;
        }
        match marker {
            None => {
                marker = Some(b);
                count = 1;
            }
            Some(m) if m == b => count += 1,
            Some(_) => return false,
        }
    }
    count >= 3
}

fn parse_blockquote_line(line: &str) -> Option<&str> {
    let trimmed = strip_indent_up_to_3(line);
    let rest = trimmed.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Unordered(u8),
    Ordered { delim: u8, start: u32 },
}

impl ListKind {
    const fn same_list(self, other: Self) -> bool {
        match (self, other) {
            (ListKind::Unordered(a), ListKind::Unordered(b)) => a == b,
            (ListKind::Ordered { delim: a, .. }, ListKind::Ordered { delim: b, .. }) => a == b,
            (ListKind::Unordered(_), ListKind::Ordered { .. })
            | (ListKind::Ordered { .. }, ListKind::Unordered(_)) => false,
        }
    }
}

fn parse_list_marker(line: &str) -> Option<(ListKind, &str)> {
    let trimmed = strip_indent_up_to_3(line);
    let first = *trimmed.as_bytes().first()?;
    if first == b'-' || first == b'*' || first == b'+' {
        let rest = trimmed.get(1..).unwrap_or("");
        if rest.is_empty() {
            return Some((ListKind::Unordered(first), ""));
        }
        if let Some(after_ws) = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t')) {
            return Some((ListKind::Unordered(first), after_ws.trim_start_matches(' ')));
        }
        return None;
    }
    if first.is_ascii_digit() {
        let digits = trimmed.as_bytes().iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let delim = *trimmed.as_bytes().get(digits)?;
        if delim != b'.' && delim != b')' {
            return None;
        }
        let rest = trimmed.get(digits + 1..).unwrap_or("");
        let item_text = if rest.is_empty() {
            ""
        } else if let Some(after_ws) = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t')) {
            after_ws.trim_start_matches(' ')
        } else {
            return None;
        };
        let mut start = 0u32;
        for &b in trimmed.as_bytes().get(..digits).unwrap_or(&[]) {
            start = start.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
        }
        return Some((ListKind::Ordered { delim, start }, item_text));
    }
    None
}

/// Zero-alloc iterator splitting a GFM pipe table line into trimmed cell slices.
struct PipeCells<'a> {
    rest: &'a str,
    done: bool,
}

impl<'a> PipeCells<'a> {
    fn new(line: &'a str) -> Self {
        let trimmed = line.trim_matches([' ', '\t']);
        let without_leading = trimmed.strip_prefix('|').unwrap_or(trimmed);
        let without_trailing = if without_leading.ends_with('|') {
            let before = without_leading.len().saturating_sub(1);
            let escaped = before > 0 && without_leading.as_bytes().get(before - 1) == Some(&b'\\');
            if escaped {
                without_leading
            } else {
                without_leading.get(..before).unwrap_or("")
            }
        } else {
            without_leading
        };
        Self { rest: without_trailing, done: trimmed.is_empty() }
    }
}

impl<'a> Iterator for PipeCells<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let bytes = self.rest.as_bytes();
        let mut i = 0usize;
        while let Some(&b) = bytes.get(i) {
            if b == b'\\' {
                i = i.saturating_add(2);
                continue;
            }
            if b == b'|' {
                let cell = self.rest.get(..i).unwrap_or("").trim_matches([' ', '\t']);
                self.rest = self.rest.get(i + 1..).unwrap_or("");
                return Some(cell);
            }
            i += 1;
        }
        self.done = true;
        Some(self.rest.trim_matches([' ', '\t']))
    }
}

fn parse_align_cell(cell: &str) -> Option<Align> {
    if cell.is_empty() {
        return None;
    }
    let left_colon = cell.starts_with(':');
    let after_left = if left_colon { cell.get(1..).unwrap_or("") } else { cell };
    let right_colon = after_left.ends_with(':');
    let dashes = if right_colon {
        after_left.get(..after_left.len().saturating_sub(1)).unwrap_or("")
    } else {
        after_left
    };
    if dashes.is_empty() || !dashes.bytes().all(|b| b == b'-') {
        return None;
    }
    Some(match (left_colon, right_colon) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        (true, false) | (false, false) => Align::Left,
    })
}

fn count_table_separator_cols(line: &str) -> Option<usize> {
    let trimmed = line.trim_matches([' ', '\t']);
    if trimmed.is_empty() || !trimmed.contains('-') {
        return None;
    }
    let mut count = 0usize;
    for cell in PipeCells::new(trimmed) {
        let _align = parse_align_cell(cell)?;
        count += 1;
    }
    (count > 0).then_some(count)
}

fn parse_table_separator<A: Allocator>(
    line: &str,
    alloc: A,
) -> Result<Option<Vec<Align, A>>, MarkdownError> {
    let trimmed = line.trim_matches([' ', '\t']);
    if trimmed.is_empty() || !trimmed.contains('-') {
        return Ok(None);
    }
    let mut aligns = Vec::new_in(alloc);
    for cell in PipeCells::new(trimmed) {
        let Some(align) = parse_align_cell(cell) else {
            return Ok(None);
        };
        aligns.try_push(align).map_err(|r| MarkdownError::Alloc(r.why))?;
    }
    if aligns.is_empty() {
        return Ok(None);
    }
    Ok(Some(aligns))
}

fn has_table_start(header_line: &str, sep_line: Option<&str>) -> bool {
    let Some(sep) = sep_line else {
        return false;
    };
    if !header_line.contains('|') {
        return false;
    }
    let Some(sep_cols) = count_table_separator_cols(sep) else {
        return false;
    };
    PipeCells::new(header_line).count() == sep_cols
}

fn is_table_start<A: Allocator + Copy>(
    header_line: &str,
    sep_line: Option<&str>,
    alloc: A,
) -> Result<Option<Vec<Align, A>>, MarkdownError> {
    let Some(sep) = sep_line else {
        return Ok(None);
    };
    if !header_line.contains('|') {
        return Ok(None);
    }
    let Some(aligns) = parse_table_separator(sep, alloc)? else {
        return Ok(None);
    };
    let header_count = PipeCells::new(header_line).count();
    if header_count == aligns.len() && header_count > 0 {
        Ok(Some(aligns))
    } else {
        Ok(None)
    }
}

fn push_block<'a, A: Allocator>(
    blocks: &mut Vec<Block<'a, A>, A>,
    block: Block<'a, A>,
    limits: MarkdownLimits,
) -> Result<(), MarkdownError> {
    if blocks.len() >= limits.most_blocks {
        return Err(MarkdownError::TooManyBlocks { limit: limits.most_blocks });
    }
    blocks.try_push(block).map_err(|r| MarkdownError::Alloc(r.why))
}

fn push_inline<'a, A: Allocator>(
    inlines: &mut Vec<Inline<'a>, A>,
    inline: Inline<'a>,
    limits: MarkdownLimits,
) -> Result<(), MarkdownError> {
    if inlines.len() >= limits.most_inlines {
        return Err(MarkdownError::TooManyInlines { limit: limits.most_inlines });
    }
    inlines.try_push(inline).map_err(|r| MarkdownError::Alloc(r.why))
}

/// Find the byte offset of a closing exact run of `run_len` copies of `delim` starting at `from`.
fn find_exact_backticks(bytes: &[u8], from: usize, run_len: usize) -> Option<usize> {
    let mut i = from;
    while let Some(&b) = bytes.get(i) {
        if b == b'`' {
            let start = i;
            let mut count = 0usize;
            while bytes.get(i) == Some(&b'`') {
                count += 1;
                i += 1;
            }
            if count == run_len {
                return Some(start);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Find closing `]` (ignoring backslash-escaped brackets), returning its byte offset.
fn find_closing_bracket(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' {
            i = i.saturating_add(2);
            continue;
        }
        if b == b']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find matching closing `)` for a link/image destination starting right after `(`, allowing
/// balanced nested parentheses and backslash escapes.
fn find_closing_paren(bytes: &[u8], from: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = from;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' {
            i = i.saturating_add(2);
            continue;
        }
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
        i += 1;
    }
    None
}

/// Find a closing 2-byte delimiter (`**`, `__`, `~~`) starting at `from` with non-empty interior.
fn find_double_delim(bytes: &[u8], from: usize, delim: u8) -> Option<usize> {
    if bytes.get(from) == Some(&b' ') || bytes.get(from) == Some(&b'\t') {
        return None;
    }
    let mut i = from;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' {
            i = i.saturating_add(2);
            continue;
        }
        if b == delim && bytes.get(i + 1) == Some(&delim) && i > from {
            let prev = bytes.get(i - 1).copied();
            if prev != Some(b' ') && prev != Some(b'\t') {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Find a closing 1-byte delimiter (`*` or `_`) starting at `from` with non-empty interior.
fn find_single_delim(bytes: &[u8], from: usize, delim: u8) -> Option<usize> {
    if bytes.get(from) == Some(&b' ') || bytes.get(from) == Some(&b'\t') {
        return None;
    }
    let mut i = from;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' {
            i = i.saturating_add(2);
            continue;
        }
        if b == delim && i > from {
            // Do not close a single-delimiter span in the middle of a double delimiter.
            if bytes.get(i + 1) == Some(&delim) {
                i += 2;
                continue;
            }
            let prev = bytes.get(i - 1).copied();
            let next_after = bytes.get(i + 1).copied();
            let word_ok = delim != b'_' || !next_after.is_some_and(|nb| nb.is_ascii_alphanumeric());
            if prev != Some(b' ') && prev != Some(b'\t') && word_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Parse inline Markdown constructs iteratively into `out`, returning the appended `Range<usize>`.
fn parse_inlines<'a, A: Allocator>(
    text: &'a str,
    out: &mut Vec<Inline<'a>, A>,
    limits: MarkdownLimits,
) -> Result<Range<usize>, MarkdownError> {
    let start_idx = out.len();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut plain_start = 0usize;

    // Linear-time guard flags: once a scan to EOF finds no matching closer, skip future rescans.
    let mut no_backtick = false;
    let mut no_rbracket = false;
    let mut no_double_star = false;
    let mut no_double_under = false;
    let mut no_double_tilde = false;
    let mut no_single_star = false;
    let mut no_single_under = false;

    while let Some(rest) = text.get(i..) {
        if rest.is_empty() {
            break;
        }
        let first = bytes.get(i).copied().unwrap_or(0);

        // Backslash escape of ASCII punctuation.
        if first == b'\\'
            && let Some(&next_b) = bytes.get(i + 1)
            && next_b.is_ascii_punctuation()
        {
            if i > plain_start
                && let Some(plain) = text.get(plain_start..i)
            {
                push_inline(out, Inline::Text(plain), limits)?;
            }
            if let Some(ch_slice) = text.get(i + 1..i + 2) {
                push_inline(out, Inline::Text(ch_slice), limits)?;
            }
            i += 2;
            plain_start = i;
            continue;
        }

        // Inline code span: `code` or ``code``.
        if first == b'`' && !no_backtick {
            let ticks = bytes.get(i..).unwrap_or(&[]).iter().take_while(|&&b| b == b'`').count();
            if let Some(close) = find_exact_backticks(bytes, i + ticks, ticks) {
                if i > plain_start
                    && let Some(plain) = text.get(plain_start..i)
                {
                    push_inline(out, Inline::Text(plain), limits)?;
                }
                let raw = text.get(i + ticks..close).unwrap_or("");
                let code = if raw.len() >= 2
                    && raw.starts_with(' ')
                    && raw.ends_with(' ')
                    && raw.bytes().any(|b| b != b' ')
                {
                    raw.get(1..raw.len() - 1).unwrap_or(raw)
                } else {
                    raw
                };
                push_inline(out, Inline::Code(code), limits)?;
                i = close + ticks;
                plain_start = i;
                continue;
            }
            if ticks == 1 {
                no_backtick = true;
            }
            i += ticks;
            continue;
        }

        // Image: ![alt](url)
        if first == b'!' && bytes.get(i + 1) == Some(&b'[') && !no_rbracket {
            if let Some(rb) = find_closing_bracket(bytes, i + 2) {
                if bytes.get(rb + 1) == Some(&b'(')
                    && let Some(rp) = find_closing_paren(bytes, rb + 2)
                {
                    if i > plain_start
                        && let Some(plain) = text.get(plain_start..i)
                    {
                        push_inline(out, Inline::Text(plain), limits)?;
                    }
                    let alt = text.get(i + 2..rb).unwrap_or("");
                    let url = text.get(rb + 2..rp).unwrap_or("").trim();
                    push_inline(out, Inline::Image { alt, url }, limits)?;
                    i = rp + 1;
                    plain_start = i;
                    continue;
                }
            } else {
                no_rbracket = true;
            }
        }

        // Link: [text](url)
        if first == b'[' && !no_rbracket {
            if let Some(rb) = find_closing_bracket(bytes, i + 1) {
                if bytes.get(rb + 1) == Some(&b'(')
                    && let Some(rp) = find_closing_paren(bytes, rb + 2)
                {
                    if i > plain_start
                        && let Some(plain) = text.get(plain_start..i)
                    {
                        push_inline(out, Inline::Text(plain), limits)?;
                    }
                    let link_text = text.get(i + 1..rb).unwrap_or("");
                    let url = text.get(rb + 2..rp).unwrap_or("").trim();
                    push_inline(out, Inline::Link { text: link_text, url }, limits)?;
                    i = rp + 1;
                    plain_start = i;
                    continue;
                }
            } else {
                no_rbracket = true;
            }
        }

        // Bold: **text** or __text__
        if (first == b'*' && bytes.get(i + 1) == Some(&b'*') && !no_double_star)
            || (first == b'_' && bytes.get(i + 1) == Some(&b'_') && !no_double_under)
        {
            let prev_ok =
                first != b'_' || i == 0 || !bytes.get(i - 1).is_some_and(|b| b.is_ascii_alphanumeric());
            if prev_ok {
                if let Some(close) = find_double_delim(bytes, i + 2, first) {
                    if i > plain_start
                        && let Some(plain) = text.get(plain_start..i)
                    {
                        push_inline(out, Inline::Text(plain), limits)?;
                    }
                    let inner = text.get(i + 2..close).unwrap_or("");
                    push_inline(out, Inline::Bold(inner), limits)?;
                    i = close + 2;
                    plain_start = i;
                    continue;
                }
                if first == b'*' {
                    no_double_star = true;
                } else {
                    no_double_under = true;
                }
            }
        }

        // Strikethrough: ~~text~~
        if first == b'~' && bytes.get(i + 1) == Some(&b'~') && !no_double_tilde {
            if let Some(close) = find_double_delim(bytes, i + 2, b'~') {
                if i > plain_start
                    && let Some(plain) = text.get(plain_start..i)
                {
                    push_inline(out, Inline::Text(plain), limits)?;
                }
                let inner = text.get(i + 2..close).unwrap_or("");
                push_inline(out, Inline::Strikethrough(inner), limits)?;
                i = close + 2;
                plain_start = i;
                continue;
            }
            no_double_tilde = true;
        }

        // Italic: *text* or _text_
        if (first == b'*' && bytes.get(i + 1) != Some(&b'*') && !no_single_star)
            || (first == b'_' && bytes.get(i + 1) != Some(&b'_') && !no_single_under)
        {
            let prev_ok =
                first != b'_' || i == 0 || !bytes.get(i - 1).is_some_and(|b| b.is_ascii_alphanumeric());
            if prev_ok {
                if let Some(close) = find_single_delim(bytes, i + 1, first) {
                    if i > plain_start
                        && let Some(plain) = text.get(plain_start..i)
                    {
                        push_inline(out, Inline::Text(plain), limits)?;
                    }
                    let inner = text.get(i + 1..close).unwrap_or("");
                    push_inline(out, Inline::Italic(inner), limits)?;
                    i = close + 1;
                    plain_start = i;
                    continue;
                }
                if first == b'*' {
                    no_single_star = true;
                } else {
                    no_single_under = true;
                }
            }
        }

        // Advance cleanly by one Unicode scalar.
        let step = rest.chars().next().map_or(1, char::len_utf8);
        i += step;
    }

    if i > plain_start
        && let Some(plain) = text.get(plain_start..i)
    {
        push_inline(out, Inline::Text(plain), limits)?;
    }

    Ok(start_idx..out.len())
}

/// Return `true` when `line` begins a block construct that interrupts an open paragraph.
fn interrupts_paragraph(line: &str, next_line: Option<&str>) -> bool {
    is_blank(line)
        || parse_fence_open(line).is_some()
        || parse_atx_heading(line).is_some()
        || is_thematic_break(line)
        || parse_blockquote_line(line).is_some()
        || parse_list_marker(line).is_some()
        || has_table_start(line, next_line)
}

/// Parse CommonMark + GFM subset `input` into a flat [`Document`] allocated in `alloc`.
pub fn parse_in<'a, A: Allocator + Copy>(
    input: &'a str,
    limits: MarkdownLimits,
    alloc: A,
) -> Result<Document<'a, A>, MarkdownError> {
    if input.len() > limits.most_bytes {
        return Err(MarkdownError::InputTooLarge { bytes: input.len(), limit: limits.most_bytes });
    }

    let mut blocks = Vec::new_in(alloc);
    let mut inlines = Vec::new_in(alloc);
    let mut cursor = LineCursor::new(input);

    while let Some(line) = cursor.next_line() {
        if is_blank(line.text) {
            continue;
        }

        // 1. Fenced code block.
        if let Some((fence_byte, fence_len, lang)) = parse_fence_open(line.text) {
            let code_start = line.next_pos;
            let mut code_end = code_start;
            let mut has_lines = false;
            while let Some(next_l) = cursor.next_line() {
                if is_fence_close(next_l.text, fence_byte, fence_len) {
                    break;
                }
                code_end = next_l.text_end;
                has_lines = true;
            }
            let code = if has_lines && code_end >= code_start {
                input.get(code_start..code_end).unwrap_or("")
            } else {
                ""
            };
            push_block(&mut blocks, Block::CodeBlock { lang, code }, limits)?;
            continue;
        }

        // 2. ATX heading.
        if let Some((level, heading_text)) = parse_atx_heading(line.text) {
            let range = parse_inlines(heading_text, &mut inlines, limits)?;
            push_block(&mut blocks, Block::Heading { level, inlines: range }, limits)?;
            continue;
        }

        // 3. Thematic break (checked before unordered list so `- - -` / `* * *` win).
        if is_thematic_break(line.text) {
            push_block(&mut blocks, Block::ThematicBreak, limits)?;
            continue;
        }

        // 4. Block quote.
        if let Some(first_quote) = parse_blockquote_line(line.text) {
            let mut lines = Vec::new_in(alloc);
            lines.try_push(first_quote).map_err(|r| MarkdownError::Alloc(r.why))?;
            while let Some(peeked) = cursor.peek() {
                if let Some(q) = parse_blockquote_line(peeked.text) {
                    let _consumed = cursor.next_line();
                    lines.try_push(q).map_err(|r| MarkdownError::Alloc(r.why))?;
                } else {
                    break;
                }
            }
            push_block(&mut blocks, Block::BlockQuote { lines }, limits)?;
            continue;
        }

        // 5. GFM pipe table.
        if let Some(aligns) =
            is_table_start(line.text, cursor.peek().map(|l| l.text), alloc)?
        {
            let _sep_consumed = cursor.next_line();
            let num_cols = aligns.len();
            let mut headers = Vec::new_in(alloc);
            for cell in PipeCells::new(line.text).take(num_cols) {
                let r = parse_inlines(cell, &mut inlines, limits)?;
                headers.try_push(r).map_err(|r| MarkdownError::Alloc(r.why))?;
            }
            let mut rows = Vec::new_in(alloc);
            while let Some(peeked) = cursor.peek() {
                if is_blank(peeked.text)
                    || !peeked.text.contains('|')
                    || parse_fence_open(peeked.text).is_some()
                    || parse_atx_heading(peeked.text).is_some()
                    || is_thematic_break(peeked.text)
                    || parse_blockquote_line(peeked.text).is_some()
                {
                    break;
                }
                let _row_consumed = cursor.next_line();
                let mut row = Vec::new_in(alloc);
                let mut count = 0usize;
                for cell in PipeCells::new(peeked.text) {
                    if count >= num_cols {
                        break;
                    }
                    let r = parse_inlines(cell, &mut inlines, limits)?;
                    row.try_push(r).map_err(|r| MarkdownError::Alloc(r.why))?;
                    count += 1;
                }
                while count < num_cols {
                    let empty = inlines.len()..inlines.len();
                    row.try_push(empty).map_err(|r| MarkdownError::Alloc(r.why))?;
                    count += 1;
                }
                rows.try_push(row).map_err(|r| MarkdownError::Alloc(r.why))?;
            }
            push_block(&mut blocks, Block::Table { headers, aligns, rows }, limits)?;
            continue;
        }

        // 6. Ordered or unordered list.
        if let Some((kind, first_item)) = parse_list_marker(line.text) {
            let (ordered, start) = match kind {
                ListKind::Unordered(_) => (false, 1u32),
                ListKind::Ordered { start, .. } => (true, start),
            };
            let mut items = Vec::new_in(alloc);
            let first_range = parse_inlines(first_item, &mut inlines, limits)?;
            items.try_push(first_range).map_err(|r| MarkdownError::Alloc(r.why))?;
            while let Some(peeked) = cursor.peek() {
                if is_thematic_break(peeked.text) {
                    break;
                }
                if let Some((next_kind, item_text)) = parse_list_marker(peeked.text)
                    && kind.same_list(next_kind)
                {
                    let _consumed = cursor.next_line();
                    let r = parse_inlines(item_text, &mut inlines, limits)?;
                    items.try_push(r).map_err(|r| MarkdownError::Alloc(r.why))?;
                } else {
                    break;
                }
            }
            push_block(&mut blocks, Block::List { ordered, start, items }, limits)?;
            continue;
        }

        // 7. Paragraph.
        let para_start = inlines.len();
        let mut cur_text = line.text;
        loop {
            let has_next = match cursor.peek() {
                Some(peeked) => {
                    let second = cursor.peek_second().map(|l| l.text);
                    !interrupts_paragraph(peeked.text, second)
                }
                None => false,
            };
            let trimmed_start = cur_text.trim_start_matches([' ', '\t']);
            let (line_body, hard_break) = if has_next {
                if let Some(before_spaces) = trimmed_start.strip_suffix("  ") {
                    (before_spaces.trim_end_matches(' '), true)
                } else if let Some(before_slash) = trimmed_start.strip_suffix('\\') {
                    (before_slash, true)
                } else {
                    (trimmed_start.trim_end_matches([' ', '\t']), false)
                }
            } else {
                (trimmed_start.trim_end_matches([' ', '\t']), false)
            };

            let _range = parse_inlines(line_body, &mut inlines, limits)?;
            if !has_next {
                break;
            }
            let brk = if hard_break { Inline::HardBreak } else { Inline::SoftBreak };
            push_inline(&mut inlines, brk, limits)?;
            let Some(next_l) = cursor.next_line() else {
                break;
            };
            cur_text = next_l.text;
        }
        let para_end = inlines.len();
        push_block(&mut blocks, Block::Paragraph { inlines: para_start..para_end }, limits)?;
    }

    Ok(Document { blocks, inlines })
}

/// Parse `input` and render it as XSS-safe HTML into a newly allocated [`blaze_own::String`].
pub fn to_html_in<A: Allocator + Copy>(
    input: &str,
    limits: MarkdownLimits,
    alloc: A,
) -> Result<String<A>, MarkdownError> {
    let doc = parse_in(input, limits, alloc)?;
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    let mut writer = TryStringWriter { out: &mut out, err: None };
    if doc.render_html_into(&mut writer).is_err()
        && let Some(why) = writer.err
    {
        return Err(MarkdownError::Alloc(why));
    }
    Ok(out)
}

/// Parse `input` and render it as ANSI-styled terminal text into a newly allocated [`blaze_own::String`].
pub fn to_ansi_in<A: Allocator + Copy>(
    input: &str,
    limits: MarkdownLimits,
    alloc: A,
) -> Result<String<A>, MarkdownError> {
    let doc = parse_in(input, limits, alloc)?;
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    let mut writer = TryStringWriter { out: &mut out, err: None };
    match doc.render_ansi_into_fallible(&mut writer) {
        Ok(()) => Ok(out),
        Err(render_ansi::AnsiRenderError::Alloc(why)) => Err(MarkdownError::Alloc(why)),
        Err(render_ansi::AnsiRenderError::Fmt(_)) => match writer.err {
            Some(why) => Err(MarkdownError::Alloc(why)),
            None => Ok(out),
        },
    }
}

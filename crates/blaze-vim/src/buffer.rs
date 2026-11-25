//! Generic [`TextBuffer`] trait and UTF-8 / line / word / delimiter navigation primitives.

use blaze_own::{AString, Alloc, String as OwnString};

use crate::Span;

/// Minimal text container interface operated on by [`crate::VimMachine`].
///
/// Any host text representation (an `AString`, `std::string::String`, rope snapshot, or
/// shell editor buffer) can implement these two methods and immediately gain full Vim editing.
pub trait TextBuffer {
    /// Current UTF-8 contents of the buffer.
    fn text(&self) -> &str;

    /// Replace byte range `start..end` (guaranteed valid UTF-8 char boundaries) with `insert`.
    fn replace_range(&mut self, start: usize, end: usize, insert: &str);
}

/// Allocator-explicit owned UTF-8 buffer for `no_std` callers and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedBuf<'a>(pub AString<'a>);

impl<'a> OwnedBuf<'a> {
    pub fn from_str_in(s: &str, alloc: Alloc<'a>) -> Self {
        Self(OwnString::from_str_in(s, alloc))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TextBuffer for OwnedBuf<'_> {
    #[inline]
    fn text(&self) -> &str {
        self.0.as_str()
    }

    fn replace_range(&mut self, start: usize, end: usize, insert: &str) {
        let s = clamp_boundary(self.0.as_str(), start);
        let e = clamp_boundary(self.0.as_str(), end).max(s);
        let alloc = *self.0.allocator();
        let old = self.0.as_str();
        let mut next = OwnString::with_capacity_in(old.len() - (e - s) + insert.len(), alloc);
        next.push_str(&old[..s]);
        next.push_str(insert);
        next.push_str(&old[e..]);
        self.0 = next;
    }
}

// =============================================================================
// UTF-8 char-boundary & line helpers
// =============================================================================

#[inline]
pub fn clamp_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[inline]
pub fn prev_char(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len()).saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[inline]
pub fn next_char(s: &str, idx: usize) -> usize {
    let len = s.len();
    let i = clamp_boundary(s, idx);
    if i >= len {
        return len;
    }
    let ch = s[i..].chars().next().unwrap();
    i + ch.len_utf8()
}

#[inline]
pub fn char_at(s: &str, idx: usize) -> Option<char> {
    let i = clamp_boundary(s, idx);
    s.get(i..)?.chars().next()
}

/// Byte span `[start, end)` of the line containing `idx` (excluding trailing `\n`).
pub fn line_bounds(s: &str, idx: usize) -> Span {
    let i = clamp_boundary(s, idx);
    let start = s[..i].rfind('\n').map_or(0, |p| p + 1);
    let end = s[i..].find('\n').map_or(s.len(), |r| i + r);
    Span::new(start, end)
}

/// Byte span `[start, end)` of the full line including its trailing `\n` (or preceding `\n` on the last line).
pub fn line_bounds_with_newline(s: &str, idx: usize) -> Span {
    let lb = line_bounds(s, idx);
    if lb.end < s.len() && s.as_bytes()[lb.end] == b'\n' {
        Span::new(lb.start, lb.end + 1)
    } else if lb.start > 0 && s.as_bytes()[lb.start - 1] == b'\n' {
        Span::new(lb.start - 1, lb.end)
    } else {
        lb
    }
}

/// Byte offset of first non-blank character on the line containing `idx` (or line end if blank).
pub fn first_non_blank(s: &str, idx: usize) -> usize {
    let lb = line_bounds(s, idx);
    for (rel, ch) in s[lb.start..lb.end].char_indices() {
        if !matches!(ch, ' ' | '\t') {
            return lb.start + rel;
        }
    }
    lb.start
}

/// Last valid cursor position on the line containing `idx` in Normal mode (last char, or `lb.start` if empty).
pub fn normal_line_last_char(s: &str, idx: usize) -> usize {
    let lb = line_bounds(s, idx);
    if lb.end > lb.start { prev_char(s, lb.end) } else { lb.start }
}

/// Clamp `idx` to a valid Normal-mode cursor offset on its current line (never sitting on `\n`
/// unless the line is empty, and never at `s.len()` unless the line is empty).
pub fn clamp_normal_cursor(s: &str, idx: usize) -> usize {
    if s.is_empty() {
        return 0;
    }
    let i = clamp_boundary(s, idx);
    let lb = line_bounds(s, i);
    if lb.start == lb.end {
        lb.start
    } else if i >= lb.end {
        prev_char(s, lb.end)
    } else {
        i
    }
}

/// Byte offset of character column `col` within line slice `line`.
pub fn nth_char_offset(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(idx, _)| idx)
}

/// 0-based `(line, col_chars)` of byte offset `idx`.
pub fn line_col(s: &str, idx: usize) -> (usize, usize) {
    let i = clamp_boundary(s, idx);
    let before = &s[..i];
    let line = before.bytes().filter(|&b| b == b'\n').count();
    let lb = line_bounds(s, i);
    let col = s[lb.start..i].chars().count();
    (line, col)
}

/// Move up or down `delta` lines while attempting to preserve `target_col`.
pub fn move_vertical(s: &str, idx: usize, delta: isize, target_col: usize, normal_clamp: bool) -> usize {
    let (cur_line, _) = line_col(s, idx);
    let total_lines = s.bytes().filter(|&b| b == b'\n').count() + 1;
    let next_line = if delta < 0 {
        cur_line.saturating_sub(delta.unsigned_abs())
    } else {
        (cur_line + delta as usize).min(total_lines - 1)
    };
    let mut start = 0usize;
    for (li, part) in s.split('\n').enumerate() {
        if li == next_line {
            let off = nth_char_offset(part, target_col);
            let raw = start + off;
            return if normal_clamp { clamp_normal_cursor(s, raw) } else { raw };
        }
        start += part.len() + 1;
    }
    if normal_clamp { clamp_normal_cursor(s, s.len()) } else { s.len() }
}

// =============================================================================
// Vim word / WORD classification & motions
// =============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CharClass {
    Whitespace,
    Word,
    Punct,
}

#[inline]
pub fn classify(ch: char, big_word: bool) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Whitespace
    } else if big_word || ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Vim `w` / `W`: forward to the start of the next word / WORD.
pub fn motion_word_forward(s: &str, idx: usize, big_word: bool) -> usize {
    let len = s.len();
    let mut i = clamp_boundary(s, idx);
    if i >= len {
        return len;
    }
    let start_cls = classify(char_at(s, i).unwrap(), big_word);
    if start_cls != CharClass::Whitespace {
        while i < len {
            let c = char_at(s, i).unwrap();
            if classify(c, big_word) != start_cls {
                break;
            }
            i = next_char(s, i);
        }
    }
    while i < len {
        let c = char_at(s, i).unwrap();
        if !c.is_whitespace() {
            break;
        }
        i = next_char(s, i);
    }
    i
}

/// Vim `b` / `B`: backward to the start of the current or previous word / WORD.
pub fn motion_word_backward(s: &str, idx: usize, big_word: bool) -> usize {
    let mut i = clamp_boundary(s, idx);
    if i == 0 {
        return 0;
    }
    i = prev_char(s, i);
    while i > 0 && char_at(s, i).is_some_and(char::is_whitespace) {
        i = prev_char(s, i);
    }
    let cls = match char_at(s, i) {
        Some(c) => classify(c, big_word),
        None => return 0,
    };
    while i > 0 {
        let p = prev_char(s, i);
        if char_at(s, p).map(|c| classify(c, big_word)) != Some(cls) {
            break;
        }
        i = p;
    }
    i
}

/// Vim `e` / `E`: forward to the end (last char) of the current or next word / WORD.
pub fn motion_word_end(s: &str, idx: usize, big_word: bool) -> usize {
    let len = s.len();
    let mut i = clamp_boundary(s, idx);
    if i >= len {
        return len;
    }
    let next = next_char(s, i);
    if next >= len {
        return i;
    }
    i = next;
    while i < len && char_at(s, i).is_some_and(char::is_whitespace) {
        i = next_char(s, i);
    }
    if i >= len {
        return prev_char(s, len);
    }
    let cls = classify(char_at(s, i).unwrap(), big_word);
    loop {
        let n = next_char(s, i);
        if n >= len || char_at(s, n).map(|c| classify(c, big_word)) != Some(cls) {
            return i;
        }
        i = n;
    }
}

/// Vim `ge` / `gE`: backward to the end of the previous word / WORD.
pub fn motion_word_end_backward(s: &str, idx: usize, big_word: bool) -> usize {
    let mut i = clamp_boundary(s, idx);
    if i == 0 {
        return 0;
    }
    let start_cls = char_at(s, i).map(|c| classify(c, big_word));
    if let Some(cls) = start_cls
        && cls != CharClass::Whitespace
    {
        // Step past the beginning of the current word.
        while i > 0 {
            let p = prev_char(s, i);
            if char_at(s, p).map(|c| classify(c, big_word)) != Some(cls) {
                i = p;
                break;
            }
            i = p;
            if i == 0 {
                return 0;
            }
        }
    } else {
        i = prev_char(s, i);
    }
    while i > 0 && char_at(s, i).is_some_and(char::is_whitespace) {
        i = prev_char(s, i);
    }
    i
}

/// Vim `%`: jump to matching delimiter `()`, `[]`, `{}`, `<>` on or after cursor on the current line.
pub fn motion_match_bracket(s: &str, idx: usize) -> Option<usize> {
    let lb = line_bounds(s, idx);
    let mut i = clamp_boundary(s, idx);
    let mut open_pos = None;
    while i < lb.end {
        if let Some(ch) = char_at(s, i)
            && matches!(ch, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>')
        {
            open_pos = Some((i, ch));
            break;
        }
        i = next_char(s, i);
    }
    let (pos, ch) = open_pos?;
    match ch {
        '(' | '[' | '{' | '<' => {
            let close = matching_close(ch);
            let mut depth = 1usize;
            let mut p = next_char(s, pos);
            while p < s.len() {
                if let Some(c) = char_at(s, p) {
                    if c == ch {
                        depth += 1;
                    } else if c == close {
                        depth -= 1;
                        if depth == 0 {
                            return Some(p);
                        }
                    }
                }
                p = next_char(s, p);
            }
            None
        }
        ')' | ']' | '}' | '>' => {
            let open = matching_open(ch);
            let mut depth = 1usize;
            let mut p = pos;
            while p > 0 {
                p = prev_char(s, p);
                if let Some(c) = char_at(s, p) {
                    if c == ch {
                        depth += 1;
                    } else if c == open {
                        depth -= 1;
                        if depth == 0 {
                            return Some(p);
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

#[inline]
pub const fn matching_close(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        other => other,
    }
}

#[inline]
pub const fn matching_open(close: char) -> char {
    match close {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '>' => '<',
        other => other,
    }
}

/// Paragraph motion: `{` (backward) or `}` (forward) to blank-line boundary.
pub fn motion_paragraph(s: &str, idx: usize, forward: bool) -> usize {
    if s.is_empty() {
        return 0;
    }
    if forward {
        let mut lb = line_bounds(s, idx);
        // Skip current blank lines, then skip non-blank lines until next blank line or EOF.
        let mut seen_non_blank = !s[lb.start..lb.end].trim().is_empty();
        while lb.end < s.len() {
            let next_start = lb.end + 1;
            lb = line_bounds(s, next_start);
            let is_blank = s[lb.start..lb.end].trim().is_empty();
            if is_blank && seen_non_blank {
                return lb.start;
            }
            if !is_blank {
                seen_non_blank = true;
            }
        }
        s.len()
    } else {
        let mut lb = line_bounds(s, idx);
        let mut seen_non_blank = !s[lb.start..lb.end].trim().is_empty();
        while lb.start > 0 {
            let prev_end = lb.start - 1;
            lb = line_bounds(s, prev_end);
            let is_blank = s[lb.start..lb.end].trim().is_empty();
            if is_blank && seen_non_blank {
                return lb.start;
            }
            if !is_blank {
                seen_non_blank = true;
            }
        }
        0
    }
}
